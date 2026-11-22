//! Screen previews: the still picture a sharer sends every few minutes so the
//! others see what is live before watching it.
//!
//! - From the capture: a small NV12 picture (the pipeline scales one frame on
//!   the GPU and reads only that back: the approved exception to keeping
//!   frame pixels off the CPU), turned into JPEG with Windows' WIC.
//! - On the way: sealed end to end for the room's participants
//!   ([`identity::seal`]), base64 for the room's JSON. The room server
//!   relays it without being able to read it.
//! - On arrival: opened, decoded (WIC) into RGBA for a texture.

use std::io;

use identity::seal::{self, Sealed};
use identity::{Identity, PublicKey};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR,
    GUID_WICPixelFormat32bppRGBA, IWICImagingFactory, WICBitmapDitherTypeNone,
    WICBitmapEncoderNoCache, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::PROPBAG2;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, STATFLAG_NONAME, STREAM_SEEK_SET,
};
use windows::Win32::System::Variant::{VARIANT, VT_R4};
use windows::Win32::UI::Shell::SHCreateMemStream;

/// The preview's largest size (the picture keeps the screen's aspect).
pub const SIZE: (u32, u32) = (320, 180);
/// JPEG quality of previews (0 to 1).
const JPEG_QUALITY: f32 = 0.7;
/// Pictures larger than this are refused on arrival.
const MAX_DECODED: (u32, u32) = (640, 360);

/// The preview size for a `width` x `height` screen: the largest that fits
/// in [`SIZE`] with its aspect ratio, in even pixels (NV12 needs them).
pub fn size_for(width: u32, height: u32) -> (u32, u32) {
    let (w, h) = (f64::from(width.max(2)), f64::from(height.max(2)));
    let scale = (f64::from(SIZE.0) / w).min(f64::from(SIZE.1) / h);
    let even = |v: f64| ((v * scale / 2.0).round() as u32 * 2).max(2);
    (even(w), even(h))
}

/// How often a sharer sends a fresh preview by default (also when it starts).
pub const INTERVAL: std::time::Duration = std::time::Duration::from_secs(180);
/// Someone coming online gets one at once, but not more often than this.
pub const MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// The first preview waits this long after sharing starts (the capture has
/// frames by then).
const FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// When a sharer takes the next preview picture.
#[derive(Debug)]
pub struct Schedule {
    /// None: no previews (off, or not sharing in a room).
    interval: Option<std::time::Duration>,
    next: std::time::Instant,
    last: Option<std::time::Instant>,
}

impl Schedule {
    /// Starting now, one every `interval` (None: never).
    pub fn new(start: std::time::Instant, interval: Option<std::time::Duration>) -> Self {
        Self {
            interval,
            next: start + FIRST_DELAY,
            last: None,
        }
    }

    /// Whether to take one now: the periodic one is due, or someone came
    /// online (`newcomer`) and the last one is older than [`MIN_INTERVAL`].
    pub fn due(&mut self, now: std::time::Instant, newcomer: bool) -> bool {
        let Some(interval) = self.interval else {
            return false;
        };
        let periodic = now >= self.next;
        let for_newcomer = newcomer
            && self
                .last
                .is_none_or(|last| now.duration_since(last) >= MIN_INTERVAL);
        if periodic || for_newcomer {
            self.last = Some(now);
            self.next = now + interval;
        }
        periodic || for_newcomer
    }
}

/// What sealed previews are bound to: the room they belong to.
fn context(room: &str) -> Vec<u8> {
    let mut context = b"ziz preview v1\0".to_vec();
    context.extend_from_slice(room.as_bytes());
    context
}

/// Seals `jpeg` for `recipients` of `room` and encodes it for the room.
pub fn seal_for_room(
    identity: &Identity,
    room: &str,
    recipients: &[PublicKey],
    jpeg: &[u8],
) -> io::Result<String> {
    let sealed = seal::seal(identity, recipients, &context(room), jpeg)?;
    Ok(base64_encode(&sealed.to_bytes()))
}

/// Opens a preview `from` sent in `room`; None if it is not for us, altered,
/// from someone else or malformed.
pub fn open_from_room(
    identity: &Identity,
    room: &str,
    from: &PublicKey,
    data: &str,
) -> Option<Vec<u8>> {
    let sealed = Sealed::from_bytes(&base64_decode(data)?)?;
    seal::open(identity, from, &context(room), &sealed)
}

/// 24-bit BGR rows (what the JPEG encoder takes) from an NV12 picture
/// (BT.709, limited range, as the encoder's input is converted).
pub fn bgr_from_nv12(picture: &gpu::convert::Nv12Image) -> Vec<u8> {
    let (w, h) = (picture.width as usize, picture.height as usize);
    let mut out = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let luma = f32::from(picture.y[y * w + x]);
            let c = (y / 2) * w + (x / 2) * 2;
            let (cb, cr) = (
                f32::from(picture.uv[c]) - 128.0,
                f32::from(picture.uv[c + 1]) - 128.0,
            );
            let l = (luma - 16.0) * 1.164_384;
            let r = l + 1.792_741 * cr;
            let g = l - 0.213_249 * cb - 0.532_909 * cr;
            let b = l + 2.112_402 * cb;
            let i = (y * w + x) * 3;
            out[i] = b.round().clamp(0.0, 255.0) as u8;
            out[i + 1] = g.round().clamp(0.0, 255.0) as u8;
            out[i + 2] = r.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn factory() -> io::Result<IWICImagingFactory> {
    // SAFETY: plain COM creation (the calling thread has joined COM).
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
        .map_err(io::Error::other)
}

/// A JPEG of `width` x `height` 24-bit BGR rows.
pub fn jpeg_from_bgr(width: u32, height: u32, bgr: &[u8]) -> io::Result<Vec<u8>> {
    let stride = width * 3;
    if bgr.len() != (stride * height) as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pixel count does not match the size",
        ));
    }
    let factory = factory()?;
    // SAFETY (whole block): COM calls on objects created here; the pixel
    // slice outlives `WritePixels`; out-pointers are live locals.
    unsafe {
        let stream = SHCreateMemStream(None).ok_or_else(|| io::Error::other("no memory stream"))?;
        let encoder = factory
            .CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null())
            .map_err(io::Error::other)?;
        encoder
            .Initialize(&stream, WICBitmapEncoderNoCache)
            .map_err(io::Error::other)?;
        let (mut frame, mut options) = (None, None);
        encoder
            .CreateNewFrame(&mut frame, &mut options)
            .map_err(io::Error::other)?;
        let frame = frame.ok_or_else(|| io::Error::other("no frame"))?;
        // Quality 0.7: well under WIC's default size, with no visible loss
        // at preview size (keeps busy screens far from the room's limit).
        if let Some(options) = &options {
            let mut name: Vec<u16> = "ImageQuality".encode_utf16().chain([0]).collect();
            let property = PROPBAG2 {
                pstrName: windows::core::PWSTR(name.as_mut_ptr()),
                ..Default::default()
            };
            // A float VARIANT (VT_R4): plain data, nothing to free.
            let mut quality = VARIANT::default();
            (*quality.Anonymous.Anonymous).vt = VT_R4;
            (*quality.Anonymous.Anonymous).Anonymous.fltVal = JPEG_QUALITY;
            // `name` and `quality` outlive the call.
            options
                .Write(1, &property, &quality)
                .map_err(io::Error::other)?;
        }
        frame
            .Initialize(options.as_ref())
            .map_err(io::Error::other)?;
        frame.SetSize(width, height).map_err(io::Error::other)?;
        let mut format = GUID_WICPixelFormat24bppBGR;
        frame
            .SetPixelFormat(&mut format)
            .map_err(io::Error::other)?;
        if format != GUID_WICPixelFormat24bppBGR {
            return Err(io::Error::other(
                "the JPEG encoder wants another pixel format",
            ));
        }
        frame
            .WritePixels(height, stride, bgr)
            .map_err(io::Error::other)?;
        frame.Commit().map_err(io::Error::other)?;
        encoder.Commit().map_err(io::Error::other)?;
        // Read the stream back.
        let mut stat = Default::default();
        stream
            .Stat(&mut stat, STATFLAG_NONAME)
            .map_err(io::Error::other)?;
        let mut jpeg = vec![0u8; stat.cbSize as usize];
        stream
            .Seek(0, STREAM_SEEK_SET, None)
            .map_err(io::Error::other)?;
        let mut read = 0u32;
        stream
            .Read(jpeg.as_mut_ptr().cast(), jpeg.len() as u32, Some(&mut read))
            .ok()
            .map_err(io::Error::other)?;
        jpeg.truncate(read as usize);
        Ok(jpeg)
    }
}

/// A decoded preview: RGBA rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decodes a JPEG (any WIC image, in fact) into RGBA; refuses pictures larger
/// than a preview.
pub fn picture_from_jpeg(jpeg: &[u8]) -> io::Result<Picture> {
    let factory = factory()?;
    // SAFETY (whole block): COM calls on objects created here; `jpeg` is
    // copied into the memory stream; out-pointers and the output buffer are
    // live and sized for the picture.
    unsafe {
        let stream =
            SHCreateMemStream(Some(jpeg)).ok_or_else(|| io::Error::other("no memory stream"))?;
        let decoder = factory
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .map_err(io::Error::other)?;
        let frame = decoder.GetFrame(0).map_err(io::Error::other)?;
        let (mut width, mut height) = (0, 0);
        frame
            .GetSize(&mut width, &mut height)
            .map_err(io::Error::other)?;
        if width == 0 || height == 0 || width > MAX_DECODED.0 || height > MAX_DECODED.1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{width}x{height} is not a preview"),
            ));
        }
        let converter = factory.CreateFormatConverter().map_err(io::Error::other)?;
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .map_err(io::Error::other)?;
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        converter
            .CopyPixels(std::ptr::null(), width * 4, &mut rgba)
            .map_err(io::Error::other)?;
        Ok(Picture {
            width,
            height,
            rgba,
        })
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (what the room accepts).
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let value = |c: u8| BASE64.iter().position(|&b| b == c).map(|v| v as u32);
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let last = index == bytes.len() / 4 - 1;
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &chunk[..4 - pad] {
            n = (n << 6) | value(c)?;
        }
        n <<= 6 * pad as u32;
        let produced = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&produced[..3 - pad]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn com() {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
        // SAFETY: joins the MTA for this test thread (never left: the test
        // thread ends).
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    }

    #[test]
    fn previews_come_at_start_every_few_minutes_and_for_newcomers() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut s = Schedule::new(t0, Some(INTERVAL));
        assert!(!s.due(at(1), false));
        assert!(s.due(at(2), false), "first one shortly after starting");
        assert!(!s.due(at(3), false));
        // Someone comes online 10 s later: too soon after the first.
        assert!(!s.due(at(12), true));
        // Still wanted, so it goes once 30 s have passed.
        assert!(s.due(at(32), true));
        assert!(!s.due(at(100), false));
        // Then every 3 minutes from the last one.
        assert!(!s.due(at(32 + 179), false));
        assert!(s.due(at(32 + 180), false));
        // Never outside a room.
        let mut off = Schedule::new(t0, None);
        assert!(!off.due(at(500), true));
    }

    #[test]
    fn preview_sizes_keep_the_aspect_ratio() {
        assert_eq!(size_for(1280, 720), (320, 180));
        assert_eq!(size_for(2560, 1440), (320, 180));
        assert_eq!(size_for(960, 720), (240, 180));
        assert_eq!(size_for(3440, 1440), (320, 134));
        assert_eq!(size_for(0, 0), (180, 180));
    }

    #[test]
    fn base64_matches_rfc_4648() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(plain.as_bytes()), encoded);
            assert_eq!(base64_decode(encoded).as_deref(), Some(plain.as_bytes()));
        }
        assert_eq!(base64_decode("Zg="), None);
        assert_eq!(base64_decode("Z==="), None);
        assert_eq!(base64_decode("Zg==Zg=="), None);
        assert_eq!(base64_decode("Zm9!"), None);
    }

    #[test]
    fn a_preview_goes_through_jpeg_and_back() {
        com();
        // A gradient with a colored square, as BGR.
        let (w, h) = SIZE;
        let mut bgr = vec![0u8; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 3) as usize;
                let square = (100..160).contains(&x) && (60..120).contains(&y);
                bgr[i] = if square { 255 } else { (x * 255 / w) as u8 };
                bgr[i + 1] = if square { 0 } else { (y * 255 / h) as u8 };
                bgr[i + 2] = if square { 0 } else { 128 };
            }
        }
        let jpeg = jpeg_from_bgr(w, h, &bgr).unwrap();
        assert!(jpeg.starts_with(&[0xFF, 0xD8]), "not a JPEG");
        assert!(jpeg.len() < 40_000, "{} bytes", jpeg.len());
        let picture = picture_from_jpeg(&jpeg).unwrap();
        assert_eq!((picture.width, picture.height), (w, h));
        // The square's middle is blue (JPEG is lossy: roughly).
        let i = ((90 * w + 130) * 4) as usize;
        let [r, g, b] = [picture.rgba[i], picture.rgba[i + 1], picture.rgba[i + 2]];
        assert!(b > 200 && r < 60 && g < 60, "rgb {r} {g} {b}");
        println!("320x180 preview JPEG: {} bytes", jpeg.len());
    }

    #[test]
    fn garbage_and_huge_pictures_are_refused() {
        com();
        assert!(picture_from_jpeg(b"not an image").is_err());
        let big = vec![0u8; 1280 * 720 * 3];
        let jpeg = jpeg_from_bgr(1280, 720, &big).unwrap();
        assert!(picture_from_jpeg(&jpeg).is_err());
    }

    #[test]
    fn nv12_converts_to_the_right_colors() {
        // White, black and pure red (BT.709 limited) in a 2x2 block each.
        let picture = gpu::convert::Nv12Image {
            width: 6,
            height: 2,
            y: vec![235, 235, 16, 16, 63, 63, 235, 235, 16, 16, 63, 63],
            uv: vec![128, 128, 128, 128, 102, 240],
        };
        let bgr = bgr_from_nv12(&picture);
        let px = |x: usize| [bgr[x * 3 + 2], bgr[x * 3 + 1], bgr[x * 3]];
        let close = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3);
        assert!(close(px(0), [255, 255, 255]), "{:?}", px(0));
        assert!(close(px(2), [0, 0, 0]), "{:?}", px(2));
        assert!(close(px(4), [255, 0, 0]), "{:?}", px(4));
    }

    #[test]
    fn previews_open_only_for_the_room_they_were_sealed_in() {
        let (ana, bia) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let data = seal_for_room(&ana, "room1", &[*bia.public_key()], b"jpeg").unwrap();
        assert_eq!(
            open_from_room(&bia, "room1", ana.public_key(), &data).as_deref(),
            Some(&b"jpeg"[..])
        );
        assert_eq!(open_from_room(&bia, "room2", ana.public_key(), &data), None);
        assert_eq!(
            open_from_room(&bia, "room1", ana.public_key(), "AAAA"),
            None
        );
    }
}
