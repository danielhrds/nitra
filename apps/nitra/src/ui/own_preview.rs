//! Our own share's live picture on its card: the share's preview texture
//! opened on the window's device and shown by a video presenter in a child
//! window over the card, like a watched screen. The window's own drawing is
//! not redone for it: each picture is a copy on the GPU and a present.

use gpu::D3d11Device;
use gpu::convert::{PreviewReader, SharedHandle};
use present::{FrameMeta, PresenterOptions, Smoothing, VideoPresenter};
use telemetry::qpc_now;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    SWP_ASYNCWINDOWPOS, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SWP_SHOWWINDOW, SetWindowPos,
};

pub struct OwnPreview {
    handle: SharedHandle,
    reader: PreviewReader,
    presenter: VideoPresenter,
    frames: u64,
    /// Where the window was last put (None: hidden), to move it on a change.
    placed: Option<[i32; 4]>,
    /// The line shown over the picture, to set it only on a change.
    line: Option<String>,
}

impl OwnPreview {
    /// Opens the texture `handle` on `device` and a hidden video window
    /// inside `parent` (an `HWND`).
    pub fn open(device: &D3d11Device, parent: isize, handle: SharedHandle) -> Result<Self, String> {
        let reader = PreviewReader::open(device, handle).map_err(|e| e.to_string())?;
        let (width, height) = pipeline::PREVIEW_SIZE;
        let presenter = VideoPresenter::open(
            device,
            PresenterOptions {
                title: "nitra preview".to_owned(),
                width,
                height,
                borderless: true,
                overlay: true,
                parent: Some(parent),
                position: None,
                background: true,
                skip_when_hidden: true,
            },
        )
        .map_err(|e| e.to_string())?;
        // Each picture as soon as it comes: nothing to even out.
        presenter.set_smoothing(Smoothing::Minimum);
        let window = presenter.hwnd();
        // SAFETY: a valid child window of ours. Disabled: its clicks go to
        // the app's window, which keeps the keyboard. Hidden until placed.
        unsafe {
            let _ = EnableWindow(window, false);
            let _ = SetWindowPos(
                window,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_HIDEWINDOW,
            );
        }
        Ok(Self {
            handle,
            reader,
            presenter,
            frames: 0,
            placed: None,
            line: None,
        })
    }

    /// The texture this was opened from (a switch brings a new one).
    pub fn handle(&self) -> SharedHandle {
        self.handle
    }

    /// Takes the latest picture and shows it (skipped while hidden).
    pub fn next_picture(&mut self) {
        if self.placed.is_none() || !self.reader.refresh() {
            return;
        }
        self.frames += 1;
        let now = qpc_now();
        let (width, height) = pipeline::PREVIEW_SIZE;
        let meta = FrameMeta {
            frame_id: self.frames,
            captured_at: now,
            decoded_at: now,
            media_time: None,
        };
        if let Err(error) = self
            .presenter
            .present(self.reader.texture(), 0, width, height, meta)
        {
            tracing::warn!(%error, "own preview picture");
        }
    }

    /// Shows `line` over the bottom of the picture.
    pub fn set_line(&mut self, line: String) {
        if self.line.as_deref() != Some(line.as_str()) {
            self.presenter.set_overlay(Some(vec![line.clone()]));
            self.line = Some(line);
        }
    }

    /// Puts the window at `rect` (client pixels of the app's window), or
    /// hides it.
    pub fn place(&mut self, rect: Option<[i32; 4]>) {
        if rect == self.placed {
            return;
        }
        self.placed = rect;
        let window = self.presenter.hwnd();
        let flags = SWP_ASYNCWINDOWPOS | SWP_NOZORDER | SWP_NOACTIVATE;
        // SAFETY: a valid child window; asynchronous, so the presenter's
        // thread applies it.
        let _ = unsafe {
            match rect {
                Some([x, y, w, h]) => {
                    SetWindowPos(window, None, x, y, w, h, flags | SWP_SHOWWINDOW)
                }
                None => SetWindowPos(
                    window,
                    None,
                    0,
                    0,
                    0,
                    0,
                    flags | SWP_NOMOVE | SWP_NOSIZE | SWP_HIDEWINDOW,
                ),
            }
        };
    }

    pub fn close(self) {
        let _ = self.presenter.close();
    }
}
