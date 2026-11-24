//! A small text overlay (a few lines of statistics) in the bottom-left
//! corner of the back buffer.
//!
//! The text is drawn with GDI into a small CPU bitmap, at most a few times
//! per second, uploaded to a BGRA texture and copied onto the back buffer
//! with D3D11. Direct2D was the first candidate, but it requires every
//! Direct3D call made on other threads of the same device to take its lock
//! (`ID2D1Multithread`), and the capture, conversion, encode and decode
//! threads all share the device; its first draw failed with
//! `D2DERR_RECREATE_TARGET` while another thread was using the device.
//! These are overlay pixels (a few KiB of text), not video frames.

use std::time::{Duration, Instant};

use gpu::D3d11Device;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, CLIP_DEFAULT_PRECIS,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, FF_MODERN, FIXED_PITCH, FW_NORMAL, FillRect, GdiFlush, GetStockObject, HBITMAP,
    HBRUSH, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT, TextOutW,
};
use windows::core::w;

const WIDTH: u32 = 640;
const LINE_HEIGHT: u32 = 20;
const PADDING: u32 = 6;
const MAX_LINES: u32 = 6;
const HEIGHT: u32 = PADDING * 2 + LINE_HEIGHT * MAX_LINES;
/// How often the text is redrawn (it changes on every frame).
const REFRESH: Duration = Duration::from_millis(250);

pub(crate) struct Overlay {
    context: ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    dc: HDC,
    bitmap: HBITMAP,
    previous_bitmap: HGDIOBJ,
    font: HFONT,
    previous_font: HGDIOBJ,
    bits: *mut u8,
    updated: Option<Instant>,
    /// Lines in the text drawn last (the copy covers only those).
    lines: u32,
}

impl Overlay {
    pub fn new(device: &D3d11Device) -> windows::core::Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: WIDTH,
            Height: HEIGHT,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: valid device; descriptor and out-pointer are live locals.
        unsafe {
            device
                .raw()
                .CreateTexture2D(&desc, None, Some(&mut texture))
        }?;
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: WIDTH as i32,
                // Negative: top-down rows, like the texture.
                biHeight: -(HEIGHT as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: plain GDI object creation; every object is selected into
        // the memory DC here and released in `Drop`.
        unsafe {
            let dc = CreateCompatibleDC(None);
            let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let previous_bitmap = SelectObject(dc, bitmap.into());
            let font = CreateFontW(
                -16,
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                (FIXED_PITCH.0 | FF_MODERN.0) as u32,
                w!("Consolas"),
            );
            let previous_font = SelectObject(dc, font.into());
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00FF_FFFF));
            Ok(Self {
                context: device.immediate_context()?,
                texture: texture.expect("CreateTexture2D succeeded"),
                dc,
                bitmap,
                previous_bitmap,
                font,
                previous_font,
                bits: bits.cast::<u8>(),
                updated: None,
                lines: 0,
            })
        }
    }

    /// Copies the overlay onto `back_buffer` (BGRA, `target` sized) at the
    /// bottom-left corner, redrawing the text first if it is due.
    pub fn draw(
        &mut self,
        back_buffer: &ID3D11Texture2D,
        lines: &[String],
        target: (u32, u32),
    ) -> windows::core::Result<()> {
        if self.updated.is_none_or(|time| time.elapsed() >= REFRESH) {
            self.render_text(lines);
            self.updated = Some(Instant::now());
        }
        // A small window shows the part that fits (the start of each line).
        let used = PADDING * 2 + LINE_HEIGHT * self.lines;
        let (width, height) = (WIDTH.min(target.0), used.min(target.1));
        if width == 0 || height == 0 || self.lines == 0 {
            return Ok(());
        }
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        // SAFETY: same device and format; the box is clipped to the target
        // above.
        unsafe {
            self.context.CopySubresourceRegion(
                back_buffer,
                0,
                0,
                target.1 - height,
                0,
                &self.texture,
                0,
                Some(&region),
            );
        }
        Ok(())
    }

    fn render_text(&mut self, lines: &[String]) {
        let area = RECT {
            left: 0,
            top: 0,
            right: WIDTH as i32,
            bottom: HEIGHT as i32,
        };
        // SAFETY: the DC, bitmap and font are owned by `self`; GdiFlush
        // finishes the drawing before the bits are read; the bits pointer
        // covers WIDTH * HEIGHT 32-bit pixels of the DIB section.
        unsafe {
            FillRect(self.dc, &area, HBRUSH(GetStockObject(BLACK_BRUSH).0));
            for (index, line) in lines.iter().take(MAX_LINES as usize).enumerate() {
                let text: Vec<u16> = line.encode_utf16().collect();
                let top = (PADDING + LINE_HEIGHT * index as u32) as i32;
                let _ = TextOutW(self.dc, PADDING as i32, top, &text);
            }
            self.lines = lines.len().min(MAX_LINES as usize) as u32;
            let _ = GdiFlush();
            self.context
                .UpdateSubresource(&self.texture, 0, None, self.bits.cast(), WIDTH * 4, 0);
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: restores the DC's original objects, then deletes ours once.
        unsafe {
            SelectObject(self.dc, self.previous_font);
            SelectObject(self.dc, self.previous_bitmap);
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
