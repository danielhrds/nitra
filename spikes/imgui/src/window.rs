//! The main window: Win32, messages forwarded to Dear ImGui's Win32
//! backend, events recorded for the main loop.

use std::cell::Cell;

use dear_imgui_sys::backend_shim::win32::dear_imgui_backend_win32_wnd_proc_handler;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, GWL_STYLE,
    GetClientRect, GetWindowLongPtrW, GetWindowRect, HWND_TOP, IDC_ARROW, KillTimer, LoadCursorW,
    PostQuitMessage, RegisterClassW, SIZE_MINIMIZED, SWP_FRAMECHANGED, SWP_NOOWNERZORDER,
    SWP_NOZORDER, SetTimer, SetWindowLongPtrW, SetWindowPos, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_DESTROY, WM_ENTERSIZEMOVE, WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_PAINT, WM_SIZE, WM_TIMER,
    WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
};
use windows::core::w;

/// Timer that keeps frames coming while Windows runs its modal loop
/// (window being moved or resized), when the main loop cannot run.
const MODAL_TIMER: usize = 1;

thread_local! {
    static RESIZED: Cell<bool> = const { Cell::new(false) };
    static MINIMIZED: Cell<bool> = const { Cell::new(false) };
    /// Called on the modal-loop timer to draw a frame (set by the main loop).
    static MODAL_STEP: Cell<Option<fn()>> = const { Cell::new(None) };
}

pub fn take_resized() -> bool {
    RESIZED.with(|flag| flag.replace(false))
}

pub fn is_minimized() -> bool {
    MINIMIZED.with(Cell::get)
}

pub fn set_modal_step(step: fn()) {
    MODAL_STEP.with(|cell| cell.set(Some(step)));
}

pub fn create(title: &str, width: i32, height: i32) -> Result<HWND, String> {
    let style: WINDOW_STYLE = WS_OVERLAPPEDWINDOW | WS_VISIBLE;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    let title = windows::core::HSTRING::from(title);
    // SAFETY: plain window creation on this thread.
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("ziz_ui_spike_imgui"),
            ..Default::default()
        };
        let _ = RegisterClassW(&class);
        let _ = AdjustWindowRectEx(&mut rect, style, false, WINDOW_EX_STYLE(0));
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("ziz_ui_spike_imgui"),
            &title,
            style,
            120,
            120,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| e.to_string())
    }
}

pub fn client_size(window: HWND) -> (u32, u32) {
    let mut rect = RECT::default();
    // SAFETY: valid window; live out-pointer.
    let _ = unsafe { GetClientRect(window, &mut rect) };
    (
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    )
}

/// Saved style and position while in borderless fullscreen.
pub struct Fullscreen {
    style: isize,
    rect: RECT,
}

/// Borderless fullscreen on the window's monitor, and back.
pub fn toggle_fullscreen(window: HWND, saved: &mut Option<Fullscreen>) {
    // SAFETY (whole block): window owned by this thread; plain style and
    // position changes; out-pointers are live locals.
    unsafe {
        if let Some(previous) = saved.take() {
            SetWindowLongPtrW(window, GWL_STYLE, previous.style);
            let rect = previous.rect;
            let _ = SetWindowPos(
                window,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOOWNERZORDER,
            );
            return;
        }
        let style = GetWindowLongPtrW(window, GWL_STYLE);
        let mut rect = RECT::default();
        let _ = GetWindowRect(window, &mut rect);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(
            MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST),
            &mut info,
        )
        .as_bool()
        {
            return;
        }
        *saved = Some(Fullscreen { style, rect });
        SetWindowLongPtrW(window, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
        let area = info.rcMonitor;
        let _ = SetWindowPos(
            window,
            Some(HWND_TOP),
            area.left,
            area.top,
            area.right - area.left,
            area.bottom - area.top,
            SWP_FRAMECHANGED | SWP_NOOWNERZORDER,
        );
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY (whole function): called by the system on the window's thread
    // with a valid handle; the ImGui handler accepts any message.
    unsafe {
        if dear_imgui_backend_win32_wnd_proc_handler(window.0, message, wparam.0, lparam.0) != 0 {
            return LRESULT(1);
        }
        match message {
            WM_SIZE => {
                MINIMIZED.with(|flag| flag.set(wparam.0 == SIZE_MINIMIZED as usize));
                RESIZED.with(|flag| flag.set(true));
                LRESULT(0)
            }
            WM_ENTERSIZEMOVE => {
                SetTimer(Some(window), MODAL_TIMER, 8, None);
                LRESULT(0)
            }
            WM_EXITSIZEMOVE => {
                let _ = KillTimer(Some(window), MODAL_TIMER);
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == MODAL_TIMER => {
                if let Some(step) = MODAL_STEP.with(Cell::get) {
                    step();
                }
                LRESULT(0)
            }
            // Never paint with GDI over the swap chain (as VideoPresenter).
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let _ = ValidateRect(Some(window), None);
                RESIZED.with(|flag| flag.set(true));
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
