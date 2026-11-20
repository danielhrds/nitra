//! The main window, owned by the main (window) thread, which only handles
//! messages. Everything drawn — swap chain, Dear ImGui, video — belongs to
//! the render thread (`main.rs`), as in `VideoPresenter`: while Windows runs
//! its modal loop (window being moved or resized) the render thread keeps
//! drawing, and it resizes the swap chain and draws the new size at once.
//!
//! Input messages are copied into a bounded queue for the render thread,
//! which feeds them to Dear ImGui's Win32 backend there (Dear ImGui's state
//! has a single owner). The two threads share their input state
//! (`AttachThreadInput`), so the backend's key-state queries see the keys.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, GWL_STYLE,
    GetClientRect, GetWindowLongPtrW, GetWindowRect, HTCLIENT, HWND_TOP, IDC_ARROW, IDC_HAND,
    IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE,
    LoadCursorW, PostQuitMessage, RegisterClassW, SIZE_MINIMIZED, SWP_FRAMECHANGED,
    SWP_NOOWNERZORDER, SWP_NOZORDER, SetCursor, SetWindowLongPtrW, SetWindowPos, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_INPUTLANGCHANGE, WM_KEYFIRST,
    WM_KEYLAST, WM_KILLFOCUS, WM_MOUSEFIRST, WM_MOUSELAST, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE,
    WM_PAINT, WM_SETCURSOR, WM_SETFOCUS, WM_SIZE, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_POPUP,
    WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

/// WM_MOUSELEAVE (declared in the Controls module of the bindings).
const WM_MOUSELEAVE: u32 = 0x02A3;
/// Asks the window thread to toggle borderless fullscreen.
pub const WM_TOGGLE_FULLSCREEN: u32 = WM_APP + 1;
/// Input messages waiting for the render thread; older ones are dropped
/// beyond this (the render thread drains the queue every wake-up).
const MAX_QUEUED: usize = 1024;

/// What the window thread tells the render thread.
pub struct Events {
    pub resized: AtomicBool,
    pub minimized: AtomicBool,
    pub closing: AtomicBool,
    /// Dear ImGui's wanted cursor (see `set_cursor`), applied by the window
    /// thread on WM_SETCURSOR.
    cursor: AtomicU8,
    input: Mutex<VecDeque<(u32, usize, isize)>>,
    /// Auto-reset event: something above changed.
    wake: isize,
}

impl Events {
    fn new() -> Result<Self, String> {
        // SAFETY: unnamed auto-reset event, closed in Drop.
        let wake = unsafe { CreateEventW(None, false, false, None) }.map_err(|e| e.to_string())?;
        Ok(Self {
            resized: AtomicBool::new(false),
            minimized: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            // Arrow until Dear ImGui says otherwise.
            cursor: AtomicU8::new(1),
            input: Mutex::new(VecDeque::new()),
            wake: wake.0 as isize,
        })
    }

    pub fn wake_handle(&self) -> HANDLE {
        HANDLE(self.wake as *mut _)
    }

    pub fn wake(&self) {
        // SAFETY: valid event owned by self.
        let _ = unsafe { SetEvent(self.wake_handle()) };
    }

    /// Takes the queued input messages, oldest first.
    pub fn take_input(&self) -> Vec<(u32, usize, isize)> {
        self.input.lock().expect("input lock").drain(..).collect()
    }

    /// Dear ImGui cursor index (`ImGuiMouseCursor_*`, -1 = none).
    pub fn set_cursor(&self, cursor: i32) {
        self.cursor
            .store((cursor + 1).clamp(0, 255) as u8, Ordering::Relaxed);
    }

    fn queue(&self, message: u32, wparam: WPARAM, lparam: LPARAM) {
        let mut input = self.input.lock().expect("input lock");
        if input.len() >= MAX_QUEUED {
            input.pop_front();
        }
        input.push_back((message, wparam.0, lparam.0));
        drop(input);
        self.wake();
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        // SAFETY: created in `new`, closed once.
        let _ = unsafe { CloseHandle(self.wake_handle()) };
    }
}

static EVENTS: OnceLock<Arc<Events>> = OnceLock::new();

thread_local! {
    /// Window-thread state: saved placement while in fullscreen.
    static FULLSCREEN: RefCell<Option<Fullscreen>> = const { RefCell::new(None) };
    /// Called (on the window thread) when the window is asked to close; it
    /// stops the render thread before the window goes.
    static ON_CLOSE: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
}

pub fn events() -> Arc<Events> {
    Arc::clone(EVENTS.get().expect("window created"))
}

pub fn set_on_close(on_close: impl FnMut() + 'static) {
    ON_CLOSE.with(|cell| *cell.borrow_mut() = Some(Box::new(on_close)));
}

/// Creates the window hidden; the caller shows it (with or without focus).
pub fn create(title: &str, width: i32, height: i32) -> Result<HWND, String> {
    let _ = EVENTS.set(Arc::new(Events::new()?));
    let style: WINDOW_STYLE = WS_OVERLAPPEDWINDOW;
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
        .map(|window| {
            style_title_bar(window);
            window
        })
        .map_err(|e| e.to_string())
    }
}

pub fn client_size(window: HWND) -> (u32, u32) {
    let mut rect = RECT::default();
    // SAFETY: valid window (any thread may query it); live out-pointer.
    let _ = unsafe { GetClientRect(window, &mut rect) };
    (
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    )
}

/// Saved style and position while in borderless fullscreen.
struct Fullscreen {
    style: isize,
    rect: RECT,
}

/// Borderless fullscreen on the window's monitor, and back (window thread).
fn toggle_fullscreen(window: HWND) {
    let saved = FULLSCREEN.with(|cell| cell.borrow_mut().take());
    // SAFETY (whole block): window owned by this thread; plain style and
    // position changes; out-pointers are live locals. The state above is not
    // borrowed while SetWindowPos re-enters the window procedure.
    unsafe {
        if let Some(previous) = saved {
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
        FULLSCREEN.with(|cell| *cell.borrow_mut() = Some(Fullscreen { style, rect }));
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

/// Messages Dear ImGui's Win32 backend reads input from.
fn is_input(message: u32) -> bool {
    (WM_MOUSEFIRST..=WM_MOUSELAST).contains(&message)
        || (WM_KEYFIRST..=WM_KEYLAST).contains(&message)
        || matches!(
            message,
            WM_SETFOCUS
                | WM_KILLFOCUS
                | WM_MOUSELEAVE
                | WM_NCMOUSEMOVE
                | WM_NCMOUSELEAVE
                | WM_INPUTLANGCHANGE
        )
}

/// The system cursor for a Dear ImGui cursor index (+1; 0 = hidden).
fn system_cursor(index: u8) -> Option<PCWSTR> {
    // ImGuiMouseCursor_: Arrow, TextInput, ResizeAll, ResizeNS, ResizeEW,
    // ResizeNESW, ResizeNWSE, Hand, Wait, Progress, NotAllowed.
    Some(match index {
        0 => return None,
        2 => IDC_IBEAM,
        3 => IDC_SIZEALL,
        4 => IDC_SIZENS,
        5 => IDC_SIZEWE,
        6 => IDC_SIZENESW,
        7 => IDC_SIZENWSE,
        8 => IDC_HAND,
        11 => IDC_NO,
        _ => IDC_ARROW,
    })
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(events) = EVENTS.get() else {
        // SAFETY: called by the system with a valid window.
        return unsafe { DefWindowProcW(window, message, wparam, lparam) };
    };
    if is_input(message) {
        events.queue(message, wparam, lparam);
    }
    // SAFETY (whole block): called by the system on the window's thread
    // with a valid handle.
    unsafe {
        match message {
            WM_SIZE => {
                events
                    .minimized
                    .store(wparam.0 == SIZE_MINIMIZED as usize, Ordering::Relaxed);
                events.resized.store(true, Ordering::Relaxed);
                events.wake();
                LRESULT(0)
            }
            WM_SETCURSOR if u32::from(lparam.0 as u16) == HTCLIENT => {
                let cursor = system_cursor(events.cursor.load(Ordering::Relaxed))
                    .and_then(|id| LoadCursorW(None, id).ok());
                SetCursor(cursor);
                LRESULT(1)
            }
            WM_TOGGLE_FULLSCREEN => {
                toggle_fullscreen(window);
                LRESULT(0)
            }
            // Never paint with GDI over the swap chain (as VideoPresenter).
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let _ = ValidateRect(Some(window), None);
                events.resized.store(true, Ordering::Relaxed);
                events.wake();
                LRESULT(0)
            }
            WM_CLOSE => {
                // The render thread lets go of the swap chain before the
                // window it presents to is destroyed.
                let on_close = ON_CLOSE.with(|cell| cell.borrow_mut().take());
                if let Some(mut on_close) = on_close {
                    on_close();
                }
                DefWindowProcW(window, message, wparam, lparam)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

/// Paints the native title bar in the app's colors (Windows 11 DWM
/// attributes; ignored on older systems): dark mode, caption #161B2E,
/// text #E8ECF7, border #262E48. Colors are COLORREF (0x00BBGGRR).
fn style_title_bar(window: HWND) {
    use windows::Win32::Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
        DwmSetWindowAttribute,
    };
    let set = |attribute, value: u32| {
        // SAFETY: valid window; the value is a live u32 of the size passed.
        let _ = unsafe { DwmSetWindowAttribute(window, attribute, (&raw const value).cast(), 4) };
    };
    set(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
    set(DWMWA_CAPTION_COLOR, 0x002E_1B16);
    set(DWMWA_TEXT_COLOR, 0x00F7_ECE8);
    set(DWMWA_BORDER_COLOR, 0x0048_2E26);
}
