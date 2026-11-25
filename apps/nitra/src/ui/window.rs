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
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, ScreenToClient,
    ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, GWL_STYLE,
    GetClientRect, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, HICON, HTBOTTOM,
    HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTCLOSE, HTLEFT, HTMAXBUTTON, HTMINBUTTON,
    HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, HWND_TOP, IDC_ARROW, IDC_HAND, IDC_IBEAM, IDC_NO,
    IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IMAGE_ICON, IsZoomed,
    LR_DEFAULTCOLOR, LoadCursorW, LoadImageW, NCCALCSIZE_PARAMS, PostMessageW, PostQuitMessage,
    RegisterClassExW, SIZE_MAXIMIZED, SIZE_MINIMIZED, SM_CXICON, SM_CXPADDEDBORDER, SM_CXSIZEFRAME,
    SM_CXSMICON, SM_CYSIZEFRAME, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SWP_FRAMECHANGED,
    SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SetCursor, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DESTROY,
    WM_ERASEBKGND, WM_INPUTLANGCHANGE, WM_KEYFIRST, WM_KEYLAST, WM_KILLFOCUS, WM_MOUSEFIRST,
    WM_MOUSELAST, WM_NCCALCSIZE, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN,
    WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_PAINT, WM_SETCURSOR, WM_SETFOCUS, WM_SIZE,
    WNDCLASSEXW, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};
// The tray icon and its menu, and the one-window-per-profile check.
use std::sync::atomic::AtomicU32;
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFY_ICON_MESSAGE, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos,
    HWND_BROADCAST, IsIconic, LR_SHARED, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    RegisterWindowMessageW, SW_HIDE, SW_SHOW, SendMessageW, SetForegroundWindow, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_CONTEXTMENU, WM_ENDSESSION, WM_LBUTTONUP,
    WM_NULL, WM_RBUTTONUP,
};

/// WM_MOUSELEAVE (declared in the Controls module of the bindings).
const WM_MOUSELEAVE: u32 = 0x02A3;
/// Asks the window thread to toggle borderless fullscreen.
pub const WM_TOGGLE_FULLSCREEN: u32 = WM_APP + 1;
/// Input messages waiting for the render thread; older ones are dropped
/// beyond this (the render thread drains the queue every wake-up).
const MAX_QUEUED: usize = 1024;

pub const POWER_SUSPENDING: u8 = 1;
pub const POWER_RESUMED: u8 = 2;
/// Power and display messages (numbers from WinUser.h).
const WM_POWERBROADCAST: u32 = 0x0218;
const WM_DISPLAYCHANGE: u32 = 0x007E;
const PBT_APMSUSPEND: usize = 0x0004;
const PBT_APMRESUMESUSPEND: usize = 0x0007;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;

/// What the window thread tells the render thread.
pub struct Events {
    pub resized: AtomicBool,
    pub minimized: AtomicBool,
    pub closing: AtomicBool,
    pub maximized: AtomicBool,
    /// Title bar button under the mouse (see `TitleButton`), for drawing.
    pub hovered_button: AtomicU8,
    /// Height of the app's title bar, and where its buttons start (client
    /// pixels), published by the render thread for hit-testing.
    pub caption_height: AtomicI32,
    pub buttons_left: AtomicI32,
    /// The close button hides the window to the tray (from the settings).
    pub close_to_tray: AtomicBool,
    /// Hidden to the tray: nothing to draw.
    pub hidden: AtomicBool,
    /// The status line of the tray's menu ("Na sala Sexta").
    pub tray_status: Mutex<String>,
    /// The PC is about to sleep (`POWER_SUSPENDING`) or woke up
    /// (`POWER_RESUMED`); the render thread takes it (0: nothing new).
    pub power: AtomicU8,
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
            maximized: AtomicBool::new(false),
            hovered_button: AtomicU8::new(0),
            caption_height: AtomicI32::new(40),
            buttons_left: AtomicI32::new(i32::MAX),
            close_to_tray: AtomicBool::new(false),
            hidden: AtomicBool::new(false),
            tray_status: Mutex::new(String::new()),
            power: AtomicU8::new(0),
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
        // The app icon, resource 1 (build.rs), at the big and small sizes
        // Windows asks for (a missing resource leaves the generic icon).
        let icon = |size: i32| {
            LoadImageW(
                Some(instance.into()),
                // MAKEINTRESOURCE(1): an integer id passed as a name pointer.
                PCWSTR(std::ptr::without_provenance(1)),
                IMAGE_ICON,
                size,
                size,
                LR_DEFAULTCOLOR,
            )
            .ok()
            .map(|h| HICON(h.0))
            .unwrap_or_default()
        };
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            hIcon: icon(GetSystemMetrics(SM_CXICON)),
            hIconSm: icon(GetSystemMetrics(SM_CXSMICON)),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("nitra_window"),
            ..Default::default()
        };
        let _ = RegisterClassExW(&class);
        let _ = AdjustWindowRectEx(&mut rect, style, false, WINDOW_EX_STYLE(0));
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("nitra_window"),
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
        .inspect(|&window| style_frame(window))
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
            // No native frame: the client area is the whole window (the app
            // draws its own title bar). Maximized, a window reaches past the
            // monitor by its frame; keep the content on screen.
            WM_NCCALCSIZE if wparam.0 != 0 => {
                if IsZoomed(window).as_bool() {
                    let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
                    let frame_x =
                        GetSystemMetrics(SM_CXSIZEFRAME) + GetSystemMetrics(SM_CXPADDEDBORDER);
                    let frame_y =
                        GetSystemMetrics(SM_CYSIZEFRAME) + GetSystemMetrics(SM_CXPADDEDBORDER);
                    let rect = &mut params.rgrc[0];
                    rect.left += frame_x;
                    rect.right -= frame_x;
                    rect.top += frame_y;
                    rect.bottom -= frame_y;
                }
                LRESULT(0)
            }
            WM_NCHITTEST => LRESULT(hit_test(window, events, lparam) as isize),
            WM_NCMOUSEMOVE => {
                let hovered = button_for(wparam.0 as u32);
                if events.hovered_button.swap(hovered, Ordering::Relaxed) != hovered {
                    events.wake();
                }
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE | TME_NONCLIENT,
                    hwndTrack: window,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut track);
                DefWindowProcW(window, message, wparam, lparam)
            }
            WM_NCMOUSELEAVE => {
                if events.hovered_button.swap(0, Ordering::Relaxed) != 0 {
                    events.wake();
                }
                PRESSED.with(|p| p.set(0));
                DefWindowProcW(window, message, wparam, lparam)
            }
            // The title bar buttons are ours: no classic buttons drawn by
            // Windows on press; the action runs on release over the same one.
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if button_for(wparam.0 as u32) != 0 => {
                PRESSED.with(|p| p.set(button_for(wparam.0 as u32)));
                LRESULT(0)
            }
            WM_NCLBUTTONUP if button_for(wparam.0 as u32) != 0 => {
                let button = button_for(wparam.0 as u32);
                if PRESSED.with(|p| p.replace(0)) == button {
                    match button {
                        TITLE_MINIMIZE => {
                            let _ = ShowWindow(window, SW_MINIMIZE);
                        }
                        TITLE_MAXIMIZE => {
                            let zoomed = IsZoomed(window).as_bool();
                            let _ =
                                ShowWindow(window, if zoomed { SW_RESTORE } else { SW_MAXIMIZE });
                        }
                        _ => {
                            let _ = PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                    }
                }
                LRESULT(0)
            }
            WM_SIZE => {
                events
                    .minimized
                    .store(wparam.0 == SIZE_MINIMIZED as usize, Ordering::Relaxed);
                events
                    .maximized
                    .store(wparam.0 == SIZE_MAXIMIZED as usize, Ordering::Relaxed);
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
            // The close button, Alt+F4 or the taskbar's "Close": to the tray
            // when the settings say so; our own quit (`quit`) always closes.
            WM_CLOSE if wparam.0 != QUIT_WPARAM && events.close_to_tray.load(Ordering::Relaxed) => {
                hide(window, events);
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
            // The PC goes to sleep (the network goes with it) or wakes up.
            WM_POWERBROADCAST => {
                let state = match wparam.0 {
                    PBT_APMSUSPEND => POWER_SUSPENDING,
                    PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => POWER_RESUMED,
                    _ => 0,
                };
                if state != 0 {
                    events.power.store(state, Ordering::Relaxed);
                    events.wake();
                }
                LRESULT(1)
            }
            // A monitor's resolution or arrangement changed.
            WM_DISPLAYCHANGE => {
                events.resized.store(true, Ordering::Relaxed);
                events.wake();
                DefWindowProcW(window, message, wparam, lparam)
            }
            // Windows is shutting down or signing out: close for real.
            WM_ENDSESSION if wparam.0 != 0 => {
                SendMessageW(window, WM_CLOSE, Some(WPARAM(QUIT_WPARAM)), None);
                LRESULT(0)
            }
            WM_TRAY => {
                tray_event(window, events, lparam.0 as u32);
                LRESULT(0)
            }
            WM_DESTROY => {
                tray_icon(window, NIM_DELETE);
                PostQuitMessage(0);
                LRESULT(0)
            }
            // Another start of this profile asks us to show up.
            _ if message != 0 && message == SHOW_MESSAGE.load(Ordering::Relaxed) => {
                show(window, events);
                LRESULT(0)
            }
            // Explorer restarted: the tray icon is gone, add it again.
            _ if message != 0 && message == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                tray_icon(window, NIM_ADD);
                LRESULT(0)
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

/// `WM_CLOSE` with this `wParam` closes for real (the tray's "Sair", the
/// app's own timers, test scripts); a plain `WM_CLOSE` may only hide.
pub const QUIT_WPARAM: usize = 0x5A5A;
/// The tray icon's notifications.
const WM_TRAY: u32 = WM_APP + 2;
/// Tray menu items.
const MENU_OPEN: usize = 1;
const MENU_QUIT: usize = 2;
/// Registered messages: "show yourself" for this profile, and
/// Explorer's "taskbar created" (the tray icon must be added again).
static SHOW_MESSAGE: AtomicU32 = AtomicU32::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// Closes the window for real (not to the tray).
pub fn quit(window: HWND) {
    // SAFETY: posting to a window; any thread may.
    let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(QUIT_WPARAM), LPARAM(0)) };
}

/// Adds the tray icon and listens for the profile's "show yourself" (call
/// once, on the window thread, after `create`).
pub fn start_tray(window: HWND, profile_key: &str) {
    // SAFETY: plain registrations; the window belongs to this thread.
    unsafe {
        SHOW_MESSAGE.store(
            RegisterWindowMessageW(&windows::core::HSTRING::from(show_message_name(
                profile_key,
            ))),
            Ordering::Relaxed,
        );
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")),
            Ordering::Relaxed,
        );
    }
    tray_icon(window, NIM_ADD);
}

/// One window per profile. Returns a guard to keep while the window
/// runs, or None when this profile already has one: that one is asked to
/// show itself, and this start should end quietly.
pub fn single_instance(profile_key: &str) -> Option<InstanceGuard> {
    let name = windows::core::HSTRING::from(format!("Local\\nitra-window-{profile_key}"));
    // SAFETY: a named mutex owned by this process (closed by the guard).
    let mutex = unsafe { CreateMutexW(None, false, &name) }.ok()?;
    // SAFETY: plain query right after the call that set it.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // SAFETY: closing the handle we just got.
        let _ = unsafe { CloseHandle(mutex) };
        // SAFETY: registering a message, letting the other process take the
        // foreground, and broadcasting (only that profile's window reacts).
        unsafe {
            let show = RegisterWindowMessageW(&windows::core::HSTRING::from(show_message_name(
                profile_key,
            )));
            let _ = AllowSetForegroundWindow(ASFW_ANY);
            let _ = PostMessageW(Some(HWND_BROADCAST), show, WPARAM(0), LPARAM(0));
        }
        return None;
    }
    Some(InstanceGuard(mutex))
}

/// Holds the profile's window mutex while the window runs.
pub struct InstanceGuard(HANDLE);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: the handle from `single_instance`, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A short key for a profile (its data folder), for system-wide names.
pub fn profile_key(dir: &std::path::Path) -> String {
    // FNV-1a over the lowercase path: the same folder gives the same key.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in dir.to_string_lossy().to_lowercase().bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn show_message_name(profile_key: &str) -> String {
    format!("nitra-show-{profile_key}")
}

/// Adds, or removes, the tray icon (`NIM_ADD` / `NIM_DELETE`).
fn tray_icon(window: HWND, operation: NOTIFY_ICON_MESSAGE) {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    // SAFETY: plain queries and a copy into a fixed buffer; the icon is the
    // app's resource (or none), owned by the system's resource cache.
    unsafe {
        let instance = GetModuleHandleW(None).ok();
        let size = GetSystemMetrics(SM_CXSMICON);
        data.hIcon = LoadImageW(
            instance.map(Into::into),
            PCWSTR(std::ptr::without_provenance(1)),
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR | LR_SHARED,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default();
        for (slot, unit) in data.szTip.iter_mut().zip("nitra".encode_utf16()) {
            *slot = unit;
        }
        let _ = Shell_NotifyIconW(operation, &data);
    }
}

/// Shows the window again (from the tray, or another start of the profile).
fn show(window: HWND, events: &Events) {
    // SAFETY: the window belongs to this thread.
    unsafe {
        let _ = ShowWindow(
            window,
            if IsIconic(window).as_bool() {
                SW_RESTORE
            } else {
                SW_SHOW
            },
        );
        let _ = SetForegroundWindow(window);
    }
    events.hidden.store(false, Ordering::Relaxed);
    events.resized.store(true, Ordering::Relaxed);
    events.wake();
}

/// Hides the window to the tray (it keeps running: sharing, watching).
fn hide(window: HWND, events: &Events) {
    // SAFETY: the window belongs to this thread.
    let _ = unsafe { ShowWindow(window, SW_HIDE) };
    events.hidden.store(true, Ordering::Relaxed);
    events.wake();
}

/// What a click on the tray icon does: a window on screen hides to the
/// tray; a hidden or minimized one comes back (like OBS).
fn tray_click_hides(hidden: bool, minimized: bool) -> bool {
    !hidden && !minimized
}

/// The tray icon was clicked: a click hides or shows the window, a right
/// click shows the menu. (A double click is two clicks: back where it was.)
fn tray_event(window: HWND, events: &Events, event: u32) {
    match event {
        WM_LBUTTONUP => {
            // SAFETY: plain query on our window.
            let minimized = unsafe { IsIconic(window) }.as_bool();
            if tray_click_hides(events.hidden.load(Ordering::Relaxed), minimized) {
                hide(window, events);
            } else {
                show(window, events);
            }
        }
        WM_RBUTTONUP | WM_CONTEXTMENU => {
            let status = events.tray_status.lock().expect("tray status").clone();
            // SAFETY (whole block): a menu made and destroyed here; the
            // window takes the foreground first so the menu closes when
            // clicking elsewhere (documented TrackPopupMenu requirement).
            unsafe {
                let Ok(menu) = CreatePopupMenu() else {
                    return;
                };
                let status = windows::core::HSTRING::from(if status.is_empty() {
                    "nitra".to_owned()
                } else {
                    status
                });
                let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, &status);
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                let open_label =
                    windows::core::HSTRING::from(crate::i18n::tr("Abrir nitra", "Open nitra"));
                let quit_label = windows::core::HSTRING::from(crate::i18n::tr("Sair", "Quit"));
                let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN, &open_label);
                let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, &quit_label);
                let mut point = POINT::default();
                let _ = GetCursorPos(&mut point);
                let _ = SetForegroundWindow(window);
                let chosen = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                    point.x,
                    point.y,
                    None,
                    window,
                    None,
                );
                let _ = DestroyMenu(menu);
                let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
                match chosen.0 as usize {
                    MENU_OPEN => show(window, events),
                    MENU_QUIT => quit(window),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Title bar buttons (`Events::hovered_button`): none, minimize, maximize
/// or restore, close.
pub const TITLE_MINIMIZE: u8 = 1;
pub const TITLE_MAXIMIZE: u8 = 2;
pub const TITLE_CLOSE: u8 = 3;
/// Width of each title bar button, in client pixels.
pub const TITLE_BUTTON_WIDTH: i32 = 36;
/// Grab width of the window's edges for resizing.
const RESIZE_BORDER: i32 = 6;

thread_local! {
    /// The title bar button pressed (and not yet released) on the window thread.
    static PRESSED: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

fn button_for(hit: u32) -> u8 {
    match hit {
        HTMINBUTTON => TITLE_MINIMIZE,
        HTMAXBUTTON => TITLE_MAXIMIZE,
        HTCLOSE => TITLE_CLOSE,
        _ => 0,
    }
}

/// What the point in `lparam` (screen) is, for Windows: the resizing edges
/// (not when maximized), the title bar buttons (Windows 11 shows the snap
/// layouts over "maximize"), the title bar (drag, double-click), or client.
fn hit_test(window: HWND, events: &Events, lparam: LPARAM) -> u32 {
    let mut point = POINT {
        x: i32::from(lparam.0 as i16),
        y: i32::from((lparam.0 >> 16) as i16),
    };
    // SAFETY: valid window of this thread; live out-pointer.
    let _ = unsafe { ScreenToClient(window, &mut point) };
    let (width, height) = client_size(window);
    let (width, height) = (width as i32, height as i32);
    // SAFETY: valid window.
    let maximized = unsafe { IsZoomed(window) }.as_bool();
    if !maximized {
        let (left, right) = (point.x < RESIZE_BORDER, point.x >= width - RESIZE_BORDER);
        let (top, bottom) = (point.y < RESIZE_BORDER, point.y >= height - RESIZE_BORDER);
        match (left, right, top, bottom) {
            (true, _, true, _) => return HTTOPLEFT,
            (_, true, true, _) => return HTTOPRIGHT,
            (true, _, _, true) => return HTBOTTOMLEFT,
            (_, true, _, true) => return HTBOTTOMRIGHT,
            (true, ..) => return HTLEFT,
            (_, true, ..) => return HTRIGHT,
            (_, _, true, _) => return HTTOP,
            (_, _, _, true) => return HTBOTTOM,
            _ => {}
        }
    }
    if point.y >= events.caption_height.load(Ordering::Relaxed) {
        return HTCLIENT;
    }
    let buttons_left = events.buttons_left.load(Ordering::Relaxed);
    if point.x >= buttons_left {
        return match (point.x - buttons_left) / TITLE_BUTTON_WIDTH {
            0 => HTMINBUTTON,
            1 => HTMAXBUTTON,
            _ => HTCLOSE,
        };
    }
    HTCAPTION
}

/// Dark mode and the app's border color for the window frame Windows still
/// draws (the thin border and shadow; Windows 11 DWM attributes, ignored on
/// older systems). Colors are COLORREF (0x00BBGGRR).
fn style_frame(window: HWND) {
    use windows::Win32::Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute,
    };
    let set = |attribute, value: u32| {
        // SAFETY: valid window; the value is a live u32 of the size passed.
        let _ = unsafe { DwmSetWindowAttribute(window, attribute, (&raw const value).cast(), 4) };
    };
    set(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
    set(DWMWA_BORDER_COLOR, 0x0048_2E26);
    // Re-run WM_NCCALCSIZE now that it removes the native title bar.
    // SAFETY: valid window of this thread; only the frame is recomputed.
    let _ = unsafe {
        SetWindowPos(
            window,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tray_click_hides_a_shown_window_and_shows_the_others() {
        assert!(tray_click_hides(false, false));
        assert!(!tray_click_hides(true, false));
        assert!(!tray_click_hides(false, true));
        assert!(!tray_click_hides(true, true));
    }

    #[test]
    fn a_profile_key_names_the_folder_whatever_its_case() {
        let a = profile_key(std::path::Path::new(
            r"C:\Users\x\AppData\Local\nitra-dev\profiles\default",
        ));
        let b = profile_key(std::path::Path::new(
            r"c:\users\X\appdata\local\nitra-dev\profiles\DEFAULT",
        ));
        let other = profile_key(std::path::Path::new(
            r"C:\Users\x\AppData\Local\nitra-dev\profiles\test",
        ));
        assert_eq!(a, b);
        assert_ne!(a, other);
        assert_eq!(a.len(), 16);
    }
}
