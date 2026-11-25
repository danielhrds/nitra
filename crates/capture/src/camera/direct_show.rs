//! Cameras only DirectShow offers, like the OBS Virtual Camera: a small
//! graph (the camera, a sample grabber, a null renderer) running free of
//! any clock. The grabber hands each sample to the capture thread, which
//! sends it to the GPU once (see `convert`), as for Media Foundation.
//!
//! The grabber (qedit.dll, part of Windows) has no bindings in the
//! `windows` crate: its two interfaces are declared here from qedit.h.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use telemetry::qpc_now;
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Media::DirectShow::{
    EC_DEVICE_LOST, EC_ERRORABORT, IAMStreamConfig, IBaseFilter, ICaptureGraphBuilder2,
    ICreateDevEnum, IGraphBuilder, IMediaControl, IMediaEvent, IMediaFilter, IMediaSample,
    VIDEO_STREAM_CONFIG_CAPS,
};
use windows::Win32::Media::MediaFoundation::{
    AM_MEDIA_TYPE, CLSID_CaptureGraphBuilder2, CLSID_FilterGraph, CLSID_SystemDeviceEnum,
    CLSID_VideoInputDeviceCategory, FORMAT_VideoInfo, MEDIASUBTYPE_NV12, MEDIASUBTYPE_RGB24,
    MEDIASUBTYPE_RGB32, MEDIASUBTYPE_YUY2, MEDIATYPE_Video, PIN_CATEGORY_CAPTURE, VIDEOINFOHEADER,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IEnumMoniker, IMoniker,
};
use windows::Win32::System::Threading::WaitForMultipleObjects;
use windows::Win32::System::Variant::{VARIANT, VT_BSTR, VariantClear};
use windows::core::{Error, GUID, Interface, Result, w};

use super::convert::{Converter, RawFrame};
use super::{CameraMode, CameraRequest, PixelFormat, choose};
use crate::CaptureEnd;
use crate::common::{
    AcceptedSettings, Backend, Counters, Delivery, OwnedEvent, Samples, ThreadContext,
};

const CLSID_SAMPLE_GRABBER: GUID = GUID::from_u128(0xc1f400a0_3f08_11d3_9f0b_006008039e37);
const CLSID_NULL_RENDERER: GUID = GUID::from_u128(0xc1f400a4_3f08_11d3_9f0b_006008039e37);
/// `VIDEO_STREAM_CONFIG_CAPS`, filled by `GetStreamCaps` and not read.
const STREAM_CAPS_BYTES: usize = 128;

/// qedit.h, declared with its own names (hence the case).
#[allow(non_snake_case)]
mod qedit {
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Media::DirectShow::IMediaSample;
    use windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE;
    use windows::Win32::System::Threading::SetEvent;
    use windows::core::{BOOL, HRESULT, IUnknown, IUnknown_Vtbl, Interface};
    use windows_core::{implement, interface};

    /// qedit.h: the grabber's callback.
    #[interface("0579154A-2B53-4994-B0D0-E773148EFF85")]
    pub unsafe trait ISampleGrabberCB: IUnknown {
        pub fn SampleCB(&self, sample_time: f64, sample: *mut c_void) -> HRESULT;
        pub fn BufferCB(&self, sample_time: f64, buffer: *mut u8, length: i32) -> HRESULT;
    }

    /// qedit.h: the sample grabber filter.
    #[interface("6B652FFF-11FE-4fce-92AD-0266B5D7C78F")]
    pub unsafe trait ISampleGrabber: IUnknown {
        pub fn SetOneShot(&self, one_shot: BOOL) -> HRESULT;
        pub fn SetMediaType(&self, media_type: *const AM_MEDIA_TYPE) -> HRESULT;
        pub fn GetConnectedMediaType(&self, media_type: *mut AM_MEDIA_TYPE) -> HRESULT;
        pub fn SetBufferSamples(&self, buffer: BOOL) -> HRESULT;
        pub fn GetCurrentBuffer(&self, size: *mut i32, buffer: *mut i32) -> HRESULT;
        pub fn GetCurrentSample(&self, sample: *mut *mut c_void) -> HRESULT;
        /// `which`: 0 calls `SampleCB`, 1 `BufferCB`.
        pub fn SetCallback(&self, callback: *mut c_void, which: i32) -> HRESULT;
    }

    /// A sample waiting for the capture thread. DirectShow samples are
    /// free-threaded COM objects: any thread may use and release them.
    pub(super) struct WaitingSample(pub(super) IMediaSample);
    // SAFETY: see above; the sample is only used by one thread at a time
    // (behind the mutex, then by the capture thread alone).
    unsafe impl Send for WaitingSample {}

    /// The newest sample, and the event that says one came.
    pub(super) struct Handoff {
        pub(super) sample: Mutex<Option<WaitingSample>>,
        /// Owned by `DsCamera`, which stops the graph (so no more callbacks)
        /// before closing it.
        pub(super) event: isize,
    }

    /// Runs on DirectShow's streaming thread: keeps the newest sample (an
    /// older one still waiting is released, back to the camera) and wakes the
    /// capture thread.
    #[implement(ISampleGrabberCB)]
    pub(super) struct Grabbed(pub(super) Arc<Handoff>);

    impl ISampleGrabberCB_Impl for Grabbed_Impl {
        unsafe fn SampleCB(&self, _sample_time: f64, sample: *mut c_void) -> HRESULT {
            // SAFETY: DirectShow passes a live sample for the call; cloning
            // takes our own reference.
            if let Some(sample) = unsafe { IMediaSample::from_raw_borrowed(&sample) } {
                *self.0.sample.lock().expect("not poisoned") = Some(WaitingSample(sample.clone()));
                // SAFETY: a live event (see `Handoff::event`).
                let _ = unsafe { SetEvent(HANDLE(self.0.event as *mut _)) };
            }
            HRESULT(0)
        }

        unsafe fn BufferCB(&self, _sample_time: f64, _buffer: *mut u8, _length: i32) -> HRESULT {
            HRESULT(0)
        }
    }
}
use qedit::{Grabbed, Handoff, ISampleGrabber, ISampleGrabberCB, WaitingSample};

/// The names of the video devices DirectShow sees (cameras Media
/// Foundation also sees are among them). Opens none. Needs COM.
pub(crate) fn camera_names() -> Result<Vec<String>> {
    devices()?.iter().map(friendly_name).collect()
}

/// A camera being read through DirectShow, on the capture thread.
pub(crate) struct DsCamera {
    control: IMediaControl,
    events: IMediaEvent,
    grabber: ISampleGrabber,
    handoff: Arc<Handoff>,
    arrived: OwnedEvent,
    converter: Converter,
    delivery: Delivery,
    layout: Layout,
    thin: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Layout {
    format: PixelFormat,
    width: u32,
    height: u32,
    /// Bytes from one row to the next (negative: stored bottom-up).
    stride: i32,
}

impl DsCamera {
    pub fn open(
        context: &ThreadContext,
        name: &str,
        request: CameraRequest,
    ) -> Result<(Self, CameraMode)> {
        let moniker = devices()?
            .into_iter()
            .find(|moniker| friendly_name(moniker).is_ok_and(|n| n == name))
            .ok_or_else(|| Error::new(E_INVALIDARG, format!("no camera called {name:?}")))?;
        // SAFETY: plain COM object creation and binding; every pointer is
        // a live local.
        let (graph, builder, source, grabber_filter, renderer) = unsafe {
            let graph: IGraphBuilder =
                CoCreateInstance(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER)?;
            let builder: ICaptureGraphBuilder2 =
                CoCreateInstance(&CLSID_CaptureGraphBuilder2, None, CLSCTX_INPROC_SERVER)?;
            builder.SetFiltergraph(&graph)?;
            let source: IBaseFilter = moniker.BindToObject(None, None)?;
            let grabber_filter: IBaseFilter =
                CoCreateInstance(&CLSID_SAMPLE_GRABBER, None, CLSCTX_INPROC_SERVER)?;
            let renderer: IBaseFilter =
                CoCreateInstance(&CLSID_NULL_RENDERER, None, CLSCTX_INPROC_SERVER)?;
            graph.AddFilter(&source, w!("camera"))?;
            graph.AddFilter(&grabber_filter, w!("grabber"))?;
            graph.AddFilter(&renderer, w!("renderer"))?;
            (graph, builder, source, grabber_filter, renderer)
        };
        let mode = select_mode(&builder, &source, request)?;
        let grabber: ISampleGrabber = grabber_filter.cast()?;
        let wanted = AM_MEDIA_TYPE {
            majortype: MEDIATYPE_Video,
            subtype: subtype_of(mode.format),
            ..Default::default()
        };
        // SAFETY: live filters of this graph; `wanted` is a live local the
        // grabber copies.
        unsafe {
            grabber.SetMediaType(&wanted).ok()?;
            grabber.SetBufferSamples(false.into()).ok()?;
            grabber.SetOneShot(false.into()).ok()?;
            builder.RenderStream(
                Some(&PIN_CATEGORY_CAPTURE),
                &MEDIATYPE_Video,
                &source,
                &grabber_filter,
                &renderer,
            )?;
        }
        let layout = connected_layout(&grabber)?;
        let arrived = OwnedEvent::new(false)?;
        let handoff = Arc::new(Handoff {
            sample: Mutex::new(None),
            event: arrived.raw(),
        });
        let callback: ISampleGrabberCB = Grabbed(Arc::clone(&handoff)).into();
        let control: IMediaControl = graph.cast()?;
        // SAFETY: live objects of this graph; the grabber keeps its own
        // reference to the callback. No clock: samples go through as they
        // come instead of being scheduled.
        unsafe {
            grabber.SetCallback(callback.as_raw(), 0).ok()?;
            graph.cast::<IMediaFilter>()?.SetSyncSource(None)?;
        }
        let converter = Converter::new(
            &context.device,
            layout.format,
            layout.width,
            layout.height,
            false,
        )?;
        let delivery = Delivery::new(
            &context.device,
            &context.options,
            layout.width,
            layout.height,
        )?;
        // SAFETY: a fully connected graph.
        unsafe { control.Run() }?;
        tracing::info!(name, ?mode, ?layout, "camera opened (DirectShow)");
        Ok((
            Self {
                events: graph.cast()?,
                control,
                grabber,
                handoff,
                arrived,
                converter,
                delivery,
                layout,
                thin: f64::from(context.options.fps) + 0.5 < mode.fps,
            },
            mode,
        ))
    }

    fn deliver(&mut self, sample: &IMediaSample, context: &ThreadContext) -> Result<()> {
        let started = qpc_now();
        Counters::add(&context.counters.arrived, 1);
        if self.thin && !self.delivery.admit(started, &context.counters) {
            return Ok(());
        }
        // SAFETY: a sample we hold a reference to; its buffer stays put
        // while we do, and holds `GetActualDataLength` bytes.
        let data = unsafe {
            let length = usize::try_from(sample.GetActualDataLength()).unwrap_or(0);
            std::slice::from_raw_parts(sample.GetPointer()?.cast_const(), length)
        };
        let stride = self.layout.stride as isize;
        let first_row = if stride < 0 {
            (self.layout.height as usize - 1) * stride.unsigned_abs()
        } else {
            0
        };
        self.converter.convert(&RawFrame {
            data,
            first_row,
            pitch: stride,
        })?;
        if self
            .delivery
            .deliver(self.converter.target(), started, context)
        {
            self.delivery.record_cost(started);
        }
        Ok(())
    }

    /// Whether the graph reported the camera gone or failed.
    fn camera_lost(&self) -> bool {
        let (mut code, mut first, mut second) = (0_i32, 0_isize, 0_isize);
        // SAFETY: a live graph; no waiting (timeout 0); each event's
        // parameters are freed.
        while unsafe { self.events.GetEvent(&mut code, &mut first, &mut second, 0) }.is_ok() {
            let _ = unsafe { self.events.FreeEventParams(code, first, second) };
            if code as u32 == EC_ERRORABORT || code as u32 == EC_DEVICE_LOST {
                return true;
            }
        }
        false
    }
}

impl Backend for DsCamera {
    fn run(&mut self, context: &ThreadContext) -> CaptureEnd {
        let handles = [context.stop_handle(), HANDLE(self.arrived.raw() as *mut _)];
        loop {
            // SAFETY: two live events; waits for either, or 500 ms.
            let woke = unsafe { WaitForMultipleObjects(&handles, false, 500) };
            if woke == WAIT_OBJECT_0 {
                return CaptureEnd::Stopped;
            }
            if self.camera_lost() {
                return CaptureEnd::SourceClosed;
            }
            let waiting = self.handoff.sample.lock().expect("not poisoned").take();
            if let Some(WaitingSample(sample)) = waiting
                && let Err(error) = self.deliver(&sample, context)
            {
                return CaptureEnd::Failed(format!("camera frame: {error}"));
            }
        }
    }

    fn close(&mut self) {
        // SAFETY: live graph objects. Stopping ends the callbacks, so the
        // event (closed after this) is no longer set.
        unsafe {
            let _ = self.control.Stop();
            let _ = self.grabber.SetCallback(std::ptr::null_mut(), 0);
        }
        self.handoff.sample.lock().expect("not poisoned").take();
    }

    fn into_results(self) -> (Samples, AcceptedSettings) {
        (self.delivery.into_samples(), AcceptedSettings::default())
    }
}

/// Reads the capture pin's formats, picks the closest to `request` and
/// sets it.
fn select_mode(
    builder: &ICaptureGraphBuilder2,
    source: &IBaseFilter,
    request: CameraRequest,
) -> Result<CameraMode> {
    let mut config: *mut c_void = std::ptr::null_mut();
    // SAFETY: live objects; the out-pointer is a live local and receives a
    // reference we take ownership of.
    let config = unsafe {
        builder.FindInterface(
            Some(&PIN_CATEGORY_CAPTURE),
            Some(&MEDIATYPE_Video),
            source,
            &IAMStreamConfig::IID,
            &mut config,
        )?;
        IAMStreamConfig::from_raw(config)
    };
    let (mut count, mut size) = (0_i32, 0_i32);
    // SAFETY: a live interface; out-pointers are live locals.
    unsafe { config.GetNumberOfCapabilities(&mut count, &mut size) }?;
    let mut caps = vec![0_u8; (size.max(0) as usize).max(STREAM_CAPS_BYTES)];
    let mut found: Vec<(CameraMode, i32)> = Vec::new();
    for index in 0..count {
        let mut media_type = std::ptr::null_mut();
        // SAFETY: `caps` is at least the size the pin asked for; the media
        // type returned is ours to free.
        if unsafe { config.GetStreamCaps(index, &mut media_type, caps.as_mut_ptr()) }.is_err() {
            continue;
        }
        // SAFETY: a media type from GetStreamCaps, read then freed once.
        unsafe {
            if let Some(mode) = mode_of(&*media_type) {
                found.push((mode, index));
            }
            delete_media_type(media_type);
        }
    }
    let modes: Vec<CameraMode> = found.iter().map(|(mode, _)| *mode).collect();
    let chosen = choose(&modes, request)
        .ok_or_else(|| Error::new(E_FAIL, "the camera offers no format nitra can read"))?;
    let index = found
        .iter()
        .find(|(mode, _)| *mode == chosen)
        .map(|(_, index)| *index)
        .expect("the chosen mode is one of them");
    let mut media_type = std::ptr::null_mut();
    // SAFETY: as above; the type is set, then freed. `caps` holds the
    // pin's VIDEO_STREAM_CONFIG_CAPS for this type (read unaligned); the
    // format block is a whole VIDEOINFOHEADER (`mode_of` checked it), whose
    // frame interval is changed in place.
    unsafe {
        config.GetStreamCaps(index, &mut media_type, caps.as_mut_ptr())?;
        let limits = caps
            .as_ptr()
            .cast::<VIDEO_STREAM_CONFIG_CAPS>()
            .read_unaligned();
        let header = (*media_type).pbFormat.cast::<VIDEOINFOHEADER>();
        let offered = std::ptr::addr_of!((*header).AvgTimePerFrame).read_unaligned();
        // Faster than asked for (the OBS Virtual Camera offers only its
        // output rate, e.g. 60): ask for the rate we want, if the pin says
        // it can go that slow. Frames never sent cost nothing.
        let wanted = (10_000_000.0 / f64::from(request.fps)).round() as i64;
        let slower = chosen.fps > f64::from(request.fps) + 0.5
            && limits.MaxFrameInterval >= wanted
            && wanted > offered;
        let mut result = chosen;
        if slower {
            std::ptr::addr_of_mut!((*header).AvgTimePerFrame).write_unaligned(wanted);
            if config.SetFormat(media_type).is_ok() {
                result.fps = 1e7 / wanted as f64;
            } else {
                std::ptr::addr_of_mut!((*header).AvgTimePerFrame).write_unaligned(offered);
            }
        }
        let set = if result.fps == chosen.fps {
            config.SetFormat(media_type)
        } else {
            Ok(())
        };
        delete_media_type(media_type);
        set?;
        tracing::info!(?chosen, fps = result.fps, slower, "camera format set");
        Ok(result)
    }
}

/// A media type as a mode, if it is uncompressed video we can read.
///
/// # Safety
/// `media_type.pbFormat` points at `cbFormat` readable bytes.
unsafe fn mode_of(media_type: &AM_MEDIA_TYPE) -> Option<CameraMode> {
    // SAFETY: as the caller promises.
    let header = unsafe { video_info(media_type)? };
    let format = pixel_format(&media_type.subtype)?;
    let fps = if header.AvgTimePerFrame > 0 {
        1e7 / header.AvgTimePerFrame as f64
    } else {
        30.0
    };
    Some(CameraMode {
        format,
        width: header.bmiHeader.biWidth.unsigned_abs(),
        height: header.bmiHeader.biHeight.unsigned_abs(),
        fps,
    })
}

/// # Safety
/// As for [`mode_of`].
unsafe fn video_info(media_type: &AM_MEDIA_TYPE) -> Option<VIDEOINFOHEADER> {
    if media_type.formattype != FORMAT_VideoInfo
        || media_type.pbFormat.is_null()
        || (media_type.cbFormat as usize) < size_of::<VIDEOINFOHEADER>()
    {
        return None;
    }
    // SAFETY: at least a whole header's bytes, as checked (unaligned read).
    Some(unsafe {
        media_type
            .pbFormat
            .cast::<VIDEOINFOHEADER>()
            .read_unaligned()
    })
}

fn pixel_format(subtype: &GUID) -> Option<PixelFormat> {
    [
        (MEDIASUBTYPE_NV12, PixelFormat::Nv12),
        (MEDIASUBTYPE_YUY2, PixelFormat::Yuy2),
        (MEDIASUBTYPE_RGB32, PixelFormat::Rgb32),
        (MEDIASUBTYPE_RGB24, PixelFormat::Rgb24),
    ]
    .into_iter()
    .find_map(|(guid, format)| (guid == *subtype).then_some(format))
}

fn subtype_of(format: PixelFormat) -> GUID {
    match format {
        PixelFormat::Nv12 => MEDIASUBTYPE_NV12,
        PixelFormat::Yuy2 => MEDIASUBTYPE_YUY2,
        PixelFormat::Rgb32 => MEDIASUBTYPE_RGB32,
        // Never chosen here: `pixel_format` offers no MJPEG.
        PixelFormat::Rgb24 | PixelFormat::Mjpeg => MEDIASUBTYPE_RGB24,
    }
}

/// How the grabber's connected frames are laid out. RGB with a positive
/// height is stored bottom-up, its rows padded to 4 bytes; YUV is top-down.
fn connected_layout(grabber: &ISampleGrabber) -> Result<Layout> {
    let mut media_type = AM_MEDIA_TYPE::default();
    // SAFETY: a connected grabber fills `media_type`; its format block is
    // read, then freed once.
    let read = unsafe {
        grabber.GetConnectedMediaType(&mut media_type).ok()?;
        let read = video_info(&media_type).zip(pixel_format(&media_type.subtype));
        free_media_type(&mut media_type);
        read
    };
    let (header, format) = read
        .ok_or_else(|| Error::new(E_FAIL, "the camera connected in a format nitra cannot read"))?;
    let header: BITMAPINFOHEADER = header.bmiHeader;
    let width = header.biWidth.unsigned_abs();
    let height = header.biHeight.unsigned_abs();
    let stride = match format {
        PixelFormat::Nv12 => width as i32,
        PixelFormat::Yuy2 => width as i32 * 2,
        PixelFormat::Rgb32 | PixelFormat::Rgb24 | PixelFormat::Mjpeg => {
            let bytes = (width * u32::from(header.biBitCount)).div_ceil(32) * 4;
            if header.biHeight > 0 {
                -(bytes as i32)
            } else {
                bytes as i32
            }
        }
    };
    Ok(Layout {
        format,
        width,
        height,
        stride,
    })
}

/// Frees what a media type points at (`FreeMediaType` in the SDK's base
/// classes).
///
/// # Safety
/// The format block came from CoTaskMemAlloc and is not used afterwards.
unsafe fn free_media_type(media_type: &mut AM_MEDIA_TYPE) {
    if !media_type.pbFormat.is_null() {
        // SAFETY: as the caller promises.
        unsafe { CoTaskMemFree(Some(media_type.pbFormat.cast_const().cast())) };
        media_type.pbFormat = std::ptr::null_mut();
        media_type.cbFormat = 0;
    }
    // SAFETY: the reference is released once and the field is not read again.
    unsafe { std::mem::ManuallyDrop::drop(&mut media_type.pUnk) };
}

/// Frees a media type and what it points at (`DeleteMediaType`).
///
/// # Safety
/// `media_type` came from CoTaskMemAlloc (as from `GetStreamCaps`) and is
/// not used afterwards.
unsafe fn delete_media_type(media_type: *mut AM_MEDIA_TYPE) {
    if let Some(media_type) = unsafe { media_type.as_mut() } {
        // SAFETY: as the caller promises.
        unsafe {
            free_media_type(media_type);
            CoTaskMemFree(Some((media_type as *const AM_MEDIA_TYPE).cast()));
        }
    }
}

/// The video input devices DirectShow lists.
fn devices() -> Result<Vec<IMoniker>> {
    // SAFETY: plain COM object creation; out-pointers are live locals.
    let enumerator = unsafe {
        let devices: ICreateDevEnum =
            CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)?;
        let mut enumerator: Option<IEnumMoniker> = None;
        devices.CreateClassEnumerator(&CLSID_VideoInputDeviceCategory, &mut enumerator, 0)?;
        enumerator
    };
    // An empty category gives S_FALSE and no enumerator.
    let Some(enumerator) = enumerator else {
        return Ok(Vec::new());
    };
    let mut monikers = Vec::new();
    loop {
        let mut next = [None];
        // SAFETY: a live enumerator; one slot to fill.
        if unsafe { enumerator.Next(&mut next, None) }.is_err() {
            break;
        }
        match next[0].take() {
            Some(moniker) => monikers.push(moniker),
            None => break,
        }
    }
    Ok(monikers)
}

fn friendly_name(moniker: &IMoniker) -> Result<String> {
    // SAFETY: a live moniker; the property bag reads into a live VARIANT,
    // cleared once after reading.
    unsafe {
        let bag: IPropertyBag = moniker.BindToStorage(None, None)?;
        let mut value = VARIANT::default();
        bag.Read(w!("FriendlyName"), &mut value, None)?;
        let inner = &value.Anonymous.Anonymous;
        let name = (inner.vt == VT_BSTR).then(|| inner.Anonymous.bstrVal.to_string());
        let _ = VariantClear(&mut value);
        name.ok_or_else(|| Error::new(E_FAIL, "a camera without a name"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtypes_map_to_pixel_formats_and_back() {
        for format in [
            PixelFormat::Nv12,
            PixelFormat::Yuy2,
            PixelFormat::Rgb32,
            PixelFormat::Rgb24,
        ] {
            assert_eq!(pixel_format(&subtype_of(format)), Some(format));
        }
        assert_eq!(pixel_format(&GUID::zeroed()), None);
    }

    #[test]
    fn a_media_type_without_a_video_header_is_no_mode() {
        let media_type = AM_MEDIA_TYPE {
            majortype: MEDIATYPE_Video,
            subtype: MEDIASUBTYPE_YUY2,
            ..Default::default()
        };
        // SAFETY: no format block (null, size 0).
        assert_eq!(unsafe { mode_of(&media_type) }, None);
    }
}
