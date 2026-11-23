//! A small, topmost Win32 window to capture in tests and benchmarks: a solid color,
//! optionally with a bar that moves on every timer tick (16 ms requested,
//! about 40 Hz measured, limited by timer granularity), so
//! WGC keeps producing frames without any other program.

use std::cell::Cell;
use std::sync::mpsc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetMessageW, KillTimer, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetTimer, SetWindowPos, TranslateMessage, WM_CLOSE,
    WM_DESTROY, WM_ERASEBKGND, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW,
    WS_VISIBLE,
};
use windows::core::{HSTRING, PCWSTR, w};

const CLASS_NAME: PCWSTR = w!("nitra_test_window");
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 16;
const BAR_WIDTH: i32 = 48;
const BAR_STEP: i32 = 8;

#[derive(Clone, Copy)]
struct Paint {
    color: [u8; 3],
    animate: bool,
    tick: i32,
}

thread_local! {
    // One test window per thread: its paint state.
    static PAINT: Cell<Paint> = const { Cell::new(Paint { color: [0, 0, 0], animate: false, tick: 0 }) };
}

/// A visible top-level window running its own message loop on a thread.
/// Closed (and its thread joined) on drop.
pub struct TestWindow {
    window: isize,
    thread: Option<JoinHandle<()>>,
}

impl TestWindow {
    /// Opens a `width` x `height` window filled with `color` (RGB).
    pub fn open(
        title: &str,
        width: i32,
        height: i32,
        color: [u8; 3],
        animate: bool,
    ) -> Result<Self, String> {
        let title = HSTRING::from(title);
        let (sender, receiver) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("test-window".to_owned())
            .spawn(move || {
                window_thread(
                    title,
                    width,
                    height,
                    Paint {
                        color,
                        animate,
                        tick: 0,
                    },
                    sender,
                )
            })
            .map_err(|error| error.to_string())?;
        match receiver.recv() {
            Ok(Ok(window)) => Ok(Self {
                window,
                thread: Some(thread),
            }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(message)
            }
            Err(_) => Err("test window thread exited".to_owned()),
        }
    }

    pub fn hwnd(&self) -> HWND {
        HWND(self.window as *mut _)
    }

    /// Changes the outer window size.
    pub fn resize(&self, width: i32, height: i32) -> Result<(), String> {
        // SAFETY: SetWindowPos may target a window owned by another thread.
        unsafe {
            SetWindowPos(
                self.hwnd(),
                None,
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .map_err(|error| error.to_string())
    }

    /// Closes the window and waits for its thread.
    pub fn close(mut self) {
        self.close_and_join();
    }

    fn close_and_join(&mut self) {
        if let Some(thread) = self.thread.take() {
            // SAFETY: posting to a window of another thread is allowed; if the
            // window is already gone the call fails harmlessly.
            let _ = unsafe { PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            let _ = thread.join();
        }
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        self.close_and_join();
    }
}

fn window_thread(
    title: HSTRING,
    width: i32,
    height: i32,
    paint: Paint,
    ready: mpsc::SyncSender<Result<isize, String>>,
) {
    PAINT.with(|state| state.set(paint));
    // SAFETY: plain Win32 window creation on this thread. The class is
    // registered once per process; re-registration fails harmlessly.
    let created = unsafe {
        let instance = GetModuleHandleW(None).map_err(|error| error.to_string());
        instance.and_then(|instance| {
            let class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            let _ = RegisterClassW(&class);
            CreateWindowExW(
                // Topmost, so screen captures (DXGI) see it above other windows.
                WS_EX_TOPMOST,
                CLASS_NAME,
                &title,
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                width,
                height,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .map_err(|error| error.to_string())
        })
    };
    let window = match created {
        Ok(window) => window,
        Err(message) => {
            let _ = ready.send(Err(message));
            return;
        }
    };
    if paint.animate {
        // SAFETY: valid window owned by this thread.
        unsafe { SetTimer(Some(window), TIMER_ID, TIMER_MS, None) };
    }
    let _ = ready.send(Ok(window.0 as isize));

    let mut message = MSG::default();
    // SAFETY: standard message loop for windows owned by this thread.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY (whole function): called by the system on the window's own
    // thread with a valid window handle.
    unsafe {
        match message {
            WM_PAINT => {
                let state = PAINT.with(Cell::get);
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(window, &mut ps);
                let mut client = RECT::default();
                let _ = GetClientRect(window, &mut client);
                let [r, g, b] = state.color;
                let background = CreateSolidBrush(COLORREF(
                    u32::from(r) | u32::from(g) << 8 | u32::from(b) << 16,
                ));
                FillRect(dc, &client, background);
                let _ = DeleteObject(background.into());
                if state.animate && client.right > BAR_WIDTH {
                    let left = (state.tick * BAR_STEP) % (client.right - BAR_WIDTH);
                    let bar = RECT {
                        left,
                        top: client.top,
                        right: left + BAR_WIDTH,
                        bottom: client.bottom,
                    };
                    let black = CreateSolidBrush(COLORREF(0));
                    FillRect(dc, &bar, black);
                    let _ = DeleteObject(black.into());
                }
                let _ = EndPaint(window, &ps);
                LRESULT(0)
            }
            WM_TIMER => {
                PAINT.with(|state| {
                    let mut paint = state.get();
                    paint.tick = paint.tick.wrapping_add(1);
                    state.set(paint);
                });
                let _ = InvalidateRect(Some(window), None, false);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_CLOSE => {
                let _ = KillTimer(Some(window), TIMER_ID);
                let _ = DestroyWindow(window);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}
