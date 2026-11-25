//! Cameras through Media Foundation: listing them (by name, without opening
//! any), and reading one with a synchronous source reader on the capture
//! thread. Each frame arrives in memory and goes to the GPU once (see
//! `convert`).

use mf::MediaFoundation;
use telemetry::qpc_now;
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFActivate, IMFMediaBuffer, IMFMediaSource, IMFMediaType, IMFSample,
    IMFSourceReader, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_YUV_MATRIX,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF2DBuffer_LockFlags_Read,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromMediaSource,
    MFEnumDeviceSources, MFMediaType_Video, MFVideoFormat_MJPG, MFVideoFormat_NV12,
    MFVideoFormat_RGB24, MFVideoFormat_RGB32, MFVideoFormat_YUY2, MFVideoTransferMatrix_BT709,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{Error, GUID, Interface, PWSTR, Result};

use super::convert::{Converter, RawFrame};
use super::{CameraMode, CameraRequest, PixelFormat, choose};
use crate::CaptureEnd;
use crate::common::{AcceptedSettings, Backend, Counters, Delivery, Samples, ThreadContext};

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// The names of the video capture devices Media Foundation sees, in its
/// order. Opens none of them. Needs COM (MTA) and a started Media
/// Foundation on this thread.
pub(crate) fn camera_names() -> Result<Vec<String>> {
    devices()?.iter().map(friendly_name).collect()
}

/// A camera being read, on the capture thread.
pub(crate) struct MfCamera {
    source: IMFMediaSource,
    reader: IMFSourceReader,
    converter: Converter,
    delivery: Delivery,
    layout: Layout,
    /// Whether frames go through the rate limiter.
    thin: bool,
    // Last: Media Foundation shuts down after the objects above are gone.
    _media_foundation: MediaFoundation,
}

/// How the current frames are laid out.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Layout {
    format: PixelFormat,
    width: u32,
    height: u32,
    /// Bytes from one row to the next (negative: stored bottom-up).
    stride: i32,
    bt709: bool,
}

impl MfCamera {
    /// Opens the first camera called `name` in the mode closest to
    /// `request`; returns it and that mode.
    pub fn open(
        context: &ThreadContext,
        name: &str,
        request: CameraRequest,
    ) -> Result<(Self, CameraMode)> {
        let media_foundation = MediaFoundation::start()?;
        let activate = devices()?
            .into_iter()
            .find(|activate| friendly_name(activate).is_ok_and(|n| n == name))
            .ok_or_else(|| Error::new(E_INVALIDARG, format!("no camera called {name:?}")))?;
        // SAFETY: a device activation object from MFEnumDeviceSources.
        let source: IMFMediaSource = unsafe { activate.ActivateObject() }?;
        let opened = (|| {
            // SAFETY: a live media source; no attributes.
            let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, None) }?;
            let mode = select_mode(&reader, request)?;
            let layout = current_layout(&reader)?;
            let converter = Converter::new(
                &context.device,
                layout.format,
                layout.width,
                layout.height,
                layout.bt709,
            )?;
            let delivery = Delivery::new(
                &context.device,
                &context.options,
                layout.width,
                layout.height,
            )?;
            Ok((reader, mode, layout, converter, delivery))
        })();
        let (reader, mode, layout, converter, delivery) = match opened {
            Ok(parts) => parts,
            Err(error) => {
                // SAFETY: a live media source; stops the camera.
                let _ = unsafe { source.Shutdown() };
                return Err(error);
            }
        };
        tracing::info!(name, ?mode, ?layout, "camera opened");
        Ok((
            Self {
                source,
                reader,
                converter,
                delivery,
                layout,
                // A camera paces itself, and frames that come over a network
                // (the Iriun) arrive in bunches the limiter would thin: only
                // limit a camera faster than asked for.
                thin: f64::from(context.options.fps) + 0.5 < mode.fps,
                _media_foundation: media_foundation,
            },
            mode,
        ))
    }

    /// Handles one sample: rate limit, to the GPU, hand off.
    fn deliver(&mut self, sample: &IMFSample, context: &ThreadContext) -> Result<()> {
        let started = qpc_now();
        Counters::add(&context.counters.arrived, 1);
        if self.thin && !self.delivery.admit(started, &context.counters) {
            return Ok(());
        }
        // SAFETY: a live sample from the reader.
        let buffer: IMFMediaBuffer = unsafe {
            if sample.GetBufferCount()? == 1 {
                sample.GetBufferByIndex(0)?
            } else {
                sample.ConvertToContiguousBuffer()?
            }
        };
        with_frame(&buffer, &self.layout, |frame| self.converter.convert(frame))?;
        if self
            .delivery
            .deliver(self.converter.target(), started, context)
        {
            self.delivery.record_cost(started);
        }
        Ok(())
    }

    /// The camera changed its format: rebuild for the new one.
    fn reconfigure(&mut self, context: &ThreadContext) -> Result<()> {
        let layout = current_layout(&self.reader)?;
        self.converter = Converter::new(
            &context.device,
            layout.format,
            layout.width,
            layout.height,
            layout.bt709,
        )?;
        if (layout.width, layout.height) != self.delivery.size() {
            self.delivery.resize(
                &context.device,
                layout.width,
                layout.height,
                &context.counters,
            )?;
        }
        tracing::info!(?layout, "camera format changed");
        self.layout = layout;
        Ok(())
    }
}

impl Backend for MfCamera {
    fn run(&mut self, context: &ThreadContext) -> CaptureEnd {
        // A synchronous read waits for the next frame (a few tens of ms), so
        // a stop is seen within one frame of a working camera.
        while !context.stop_requested() {
            let mut flags = 0_u32;
            let mut sample: Option<IMFSample> = None;
            // SAFETY: a live reader; the out-pointers reference live locals.
            let read = unsafe {
                self.reader
                    .ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample))
            };
            if let Err(error) = read {
                return CaptureEnd::Failed(format!("camera read: {error}"));
            }
            let flags = flags as i32;
            if flags & MF_SOURCE_READERF_ERROR.0 != 0 {
                // What an unplugged camera gives.
                return CaptureEnd::SourceClosed;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 != 0 {
                return CaptureEnd::SourceClosed;
            }
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 != 0
                && let Err(error) = self.reconfigure(context)
            {
                return CaptureEnd::Failed(format!("camera format change: {error}"));
            }
            // No sample: a gap in the stream (a "tick"); keep reading.
            if let Some(sample) = sample
                && let Err(error) = self.deliver(&sample, context)
            {
                return CaptureEnd::Failed(format!("camera frame: {error}"));
            }
        }
        CaptureEnd::Stopped
    }

    fn close(&mut self) {
        // SAFETY: a live media source; stops the camera (its light goes off).
        let _ = unsafe { self.source.Shutdown() };
    }

    fn into_results(self) -> (Samples, AcceptedSettings) {
        (self.delivery.into_samples(), AcceptedSettings::default())
    }
}

/// Calls `f` with the buffer's bytes as a frame laid out as `layout` says,
/// locked only for the call.
fn with_frame(
    buffer: &IMFMediaBuffer,
    layout: &Layout,
    f: impl FnOnce(&RawFrame<'_>) -> Result<()>,
) -> Result<()> {
    if let Ok(buffer) = buffer.cast::<IMF2DBuffer2>() {
        let (mut scanline0, mut pitch) = (std::ptr::null_mut(), 0_i32);
        let (mut start, mut length) = (std::ptr::null_mut(), 0_u32);
        // SAFETY: a live 2D buffer; the out-pointers reference live locals.
        unsafe {
            buffer.Lock2DSize(
                MF2DBuffer_LockFlags_Read,
                &mut scanline0,
                &mut pitch,
                &mut start,
                &mut length,
            )
        }?;
        // SAFETY: while locked, `start` points at `length` readable bytes
        // that stay put until Unlock2D below; `scanline0` lies inside them.
        let result = unsafe {
            let data = std::slice::from_raw_parts(start.cast_const(), length as usize);
            let first_row = usize::try_from(scanline0.offset_from(start)).unwrap_or(usize::MAX);
            f(&RawFrame {
                data,
                first_row,
                pitch: pitch as isize,
            })
        };
        // SAFETY: balances the successful Lock2DSize.
        unsafe { buffer.Unlock2D() }?;
        return result;
    }
    let (mut start, mut length) = (std::ptr::null_mut(), 0_u32);
    // SAFETY: a live buffer; the out-pointers reference live locals.
    unsafe { buffer.Lock(&mut start, None, Some(&mut length)) }?;
    let stride = layout.stride as isize;
    // A bottom-up image starts with its last row.
    let first_row = if stride < 0 {
        (layout.height as usize - 1) * stride.unsigned_abs()
    } else {
        0
    };
    // SAFETY: while locked, `start` points at `length` readable bytes that
    // stay put until Unlock below.
    let result = unsafe {
        f(&RawFrame {
            data: std::slice::from_raw_parts(start.cast_const(), length as usize),
            first_row,
            pitch: stride,
        })
    };
    // SAFETY: balances the successful Lock.
    unsafe { buffer.Unlock() }?;
    result
}

/// Picks the native mode closest to `request` and sets it; MJPEG is asked
/// for as YUY2, so Media Foundation adds its decoder.
fn select_mode(reader: &IMFSourceReader, request: CameraRequest) -> Result<CameraMode> {
    let mut native = Vec::new();
    for index in 0.. {
        // SAFETY: a live reader; fails past the last type.
        let Ok(media_type) = (unsafe { reader.GetNativeMediaType(STREAM, index) }) else {
            break;
        };
        if let Some(mode) = mode_of(&media_type) {
            native.push((mode, media_type));
        }
    }
    let modes: Vec<CameraMode> = native.iter().map(|(mode, _)| *mode).collect();
    let chosen = choose(&modes, request)
        .ok_or_else(|| Error::new(E_FAIL, "the camera offers no format nitra can read"))?;
    let (_, media_type) = native
        .iter()
        .find(|(mode, _)| *mode == chosen)
        .expect("the chosen mode is one of them");
    // SAFETY: a live reader and one of its own native types.
    unsafe { reader.SetCurrentMediaType(STREAM, None, media_type) }?;
    if chosen.format == PixelFormat::Mjpeg {
        // SAFETY: a new, empty media type, filled before use.
        let decoded = unsafe { MFCreateMediaType() }?;
        // SAFETY: a live media type and reader.
        unsafe {
            decoded.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            decoded.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_YUY2)?;
            reader.SetCurrentMediaType(STREAM, None, &decoded)?;
        }
    }
    Ok(chosen)
}

/// A native media type as a mode, if it is one we can read.
fn mode_of(media_type: &IMFMediaType) -> Option<CameraMode> {
    // SAFETY: a live media type; missing attributes are errors, turned
    // into `None`.
    unsafe {
        let format = pixel_format(&media_type.GetGUID(&MF_MT_SUBTYPE).ok()?)?;
        let size = media_type.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
        let rate = media_type.GetUINT64(&MF_MT_FRAME_RATE).ok()?;
        let denominator = (rate as u32).max(1);
        Some(CameraMode {
            format,
            width: (size >> 32) as u32,
            height: size as u32,
            fps: f64::from((rate >> 32) as u32) / f64::from(denominator),
        })
    }
}

fn pixel_format(subtype: &GUID) -> Option<PixelFormat> {
    [
        (MFVideoFormat_NV12, PixelFormat::Nv12),
        (MFVideoFormat_YUY2, PixelFormat::Yuy2),
        (MFVideoFormat_RGB32, PixelFormat::Rgb32),
        (MFVideoFormat_RGB24, PixelFormat::Rgb24),
        (MFVideoFormat_MJPG, PixelFormat::Mjpeg),
    ]
    .into_iter()
    .find_map(|(guid, format)| (guid == *subtype).then_some(format))
}

/// The layout of the frames the reader gives now (after any decoder).
fn current_layout(reader: &IMFSourceReader) -> Result<Layout> {
    // SAFETY: a live reader.
    let media_type = unsafe { reader.GetCurrentMediaType(STREAM) }?;
    let mode = mode_of(&media_type)
        .filter(|mode| mode.format != PixelFormat::Mjpeg)
        .ok_or_else(|| Error::new(E_FAIL, "the camera gives frames nitra cannot read"))?;
    // RGB without a stated stride is stored bottom-up, YUV top-down (as
    // MFGetStrideForBitmapInfoHeader says).
    let row_bytes = match mode.format {
        PixelFormat::Nv12 => mode.width,
        PixelFormat::Yuy2 => mode.width * 2,
        PixelFormat::Rgb32 => mode.width * 4,
        PixelFormat::Rgb24 | PixelFormat::Mjpeg => mode.width * 3,
    } as i32;
    let rgb = matches!(mode.format, PixelFormat::Rgb32 | PixelFormat::Rgb24);
    // SAFETY: a live media type; a missing attribute is an error.
    let stride = unsafe { media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|s| s as i32)
        .unwrap_or(if rgb { -row_bytes } else { row_bytes });
    // SAFETY: as above.
    let matrix = unsafe { media_type.GetUINT32(&MF_MT_YUV_MATRIX) }.ok();
    Ok(Layout {
        format: mode.format,
        width: mode.width,
        height: mode.height,
        stride,
        bt709: matrix == Some(MFVideoTransferMatrix_BT709.0 as u32),
    })
}

/// The video capture devices, owned (each released when dropped).
fn devices() -> Result<Vec<IMFActivate>> {
    let mut attributes = None;
    // SAFETY: the out-pointer references a live local.
    unsafe { MFCreateAttributes(&mut attributes, 1) }?;
    let attributes = attributes.expect("MFCreateAttributes succeeded");
    // SAFETY: live attributes; plain GUID values.
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
    }?;
    let (mut list, mut count) = (std::ptr::null_mut(), 0_u32);
    // SAFETY: the out-pointers reference live locals.
    unsafe { MFEnumDeviceSources(&attributes, &mut list, &mut count) }?;
    let mut devices = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        // SAFETY: `list` holds `count` initialized entries; each is read
        // (moved out) exactly once, so its reference is released when the
        // Vec drops it.
        if let Some(activate) = unsafe { std::ptr::read(list.add(index)) } {
            devices.push(activate);
        }
    }
    // SAFETY: the array came from CoTaskMemAlloc; its entries were moved out.
    unsafe { CoTaskMemFree(Some(list.cast_const().cast())) };
    Ok(devices)
}

fn friendly_name(activate: &IMFActivate) -> Result<String> {
    let (mut name, mut length) = (PWSTR::null(), 0_u32);
    // SAFETY: a live activation object; the out-pointers reference live locals.
    unsafe {
        activate.GetAllocatedString(
            &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
            &mut name,
            &mut length,
        )
    }?;
    // SAFETY: a NUL-terminated string from CoTaskMemAlloc, freed once here.
    let text = unsafe { name.to_string() };
    unsafe { CoTaskMemFree(Some(name.0.cast_const().cast())) };
    text.map_err(|_| Error::new(E_FAIL, "a camera name that is not UTF-16"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtypes_map_to_pixel_formats() {
        assert_eq!(pixel_format(&MFVideoFormat_YUY2), Some(PixelFormat::Yuy2));
        assert_eq!(pixel_format(&MFVideoFormat_RGB24), Some(PixelFormat::Rgb24));
        assert_eq!(pixel_format(&MFVideoFormat_MJPG), Some(PixelFormat::Mjpeg));
        assert_eq!(pixel_format(&GUID::zeroed()), None);
    }
}
