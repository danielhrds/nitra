//! Media Foundation H.264 decoder (Microsoft's decoder MFT) with DXVA
//! hardware decoding on the caller's D3D11 device, through an
//! `IMFDXGIDeviceManager`: pictures come out as NV12 slices of a D3D11
//! texture array, never through the CPU.
//!
//! The MFT is synchronous: `ProcessInput` refuses input
//! (`MF_E_NOTACCEPTING`) while output is waiting, and `ProcessOutput`
//! returns `MF_E_TRANSFORM_NEED_MORE_INPUT` when nothing is ready, so both
//! calls are non-blocking. Low-latency mode makes it hand each picture out
//! as soon as it is decoded (no reorder buffering; our streams have no
//! B-frames).
//!
//! The caller must have initialized COM (MTA) and started Media Foundation
//! (`mf::MediaFoundation`) on the calling thread.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use gpu::D3d11Device;
use telemetry::{QpcTimestamp, qpc_now};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, IMFAttributes, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaType,
    IMFTransform, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE,
    MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFVideoArea, MFVideoFormat_H264, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::Interface;

use crate::contract::{
    DecodeError, DecodeSubmit, DecodedFrame, Pending, PendingFrame, VideoDecoder,
};

/// Sample times are in 100 ns units; consecutive inputs are one nominal
/// 60 fps frame apart (only used to match outputs with inputs).
const SAMPLE_STEP: i64 = 166_667;
/// Submitted frames remembered while waiting for their pictures.
const PENDING_CAPACITY: usize = 32;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// The picture size of the current output type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PictureSize {
    width: u32,
    height: u32,
}

pub struct MediaFoundationDecoder {
    transform: IMFTransform,
    _device_manager: IMFDXGIDeviceManager,
    size: Option<PictureSize>,
    provides_samples: bool,
    pending: Pending,
    next_sample: i64,
}

impl MediaFoundationDecoder {
    /// Creates the decoder on `device` (created with video support).
    pub fn new(device: &D3d11Device) -> Result<Self, DecodeError> {
        device.enable_multithread_protection()?;
        // SAFETY: plain COM object creation (in-process MFT).
        let transform: IMFTransform =
            unsafe { CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER) }?;
        // SAFETY: valid transform; attribute keys are static GUIDs.
        let attributes: IMFAttributes = unsafe { transform.GetAttributes() }?;
        if unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) == 0 {
            return Err(DecodeError::Unsupported(
                "the H.264 decoder MFT is not D3D11 aware".to_owned(),
            ));
        }
        // SAFETY: as above. Hand each picture out as soon as it is decoded.
        unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) }?;

        let mut reset_token = 0_u32;
        let mut device_manager = None;
        // SAFETY: out-pointers reference live locals.
        unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager) }?;
        let device_manager: IMFDXGIDeviceManager =
            device_manager.expect("MFCreateDXGIDeviceManager succeeded");
        // SAFETY: valid manager and device (video support, multithread
        // protection on); the manager keeps the device alive. With it the
        // MFT decodes with DXVA and outputs D3D11 textures.
        unsafe {
            device_manager.ResetDevice(device.raw(), reset_token)?;
            transform.ProcessMessage(
                MFT_MESSAGE_SET_D3D_MANAGER,
                device_manager.as_raw() as usize,
            )?;
        }

        // SAFETY: creates and sets media types on a valid transform.
        unsafe {
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            transform.SetInputType(0, &input, 0)?;
        }
        let mut decoder = Self {
            transform,
            _device_manager: device_manager,
            size: None,
            provides_samples: true,
            pending: Pending::new(PENDING_CAPACITY),
            next_sample: 0,
        };
        decoder.select_nv12_output()?;
        // SAFETY: valid transform; starts accepting input.
        unsafe {
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(decoder)
    }

    /// Picks the NV12 output type the decoder offers (after creation and
    /// whenever the stream format changes, e.g. on the first SPS).
    fn select_nv12_output(&mut self) -> Result<(), DecodeError> {
        let mut index = 0;
        loop {
            // SAFETY: valid transform; enumerates its preferred types.
            let offered: IMFMediaType =
                match unsafe { self.transform.GetOutputAvailableType(0, index) } {
                    Ok(offered) => offered,
                    Err(error) => {
                        return Err(DecodeError::Unsupported(format!(
                            "no NV12 output type ({error})"
                        )));
                    }
                };
            // SAFETY: valid media type; static key.
            if unsafe { offered.GetGUID(&MF_MT_SUBTYPE) }? == MFVideoFormat_NV12 {
                // SAFETY: a type the transform itself offered.
                unsafe { self.transform.SetOutputType(0, &offered, 0) }?;
                self.size = picture_size(&offered);
                // SAFETY: valid transform; stream 0 exists once types are set.
                let info = unsafe { self.transform.GetOutputStreamInfo(0) }?;
                self.provides_samples =
                    info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
                return Ok(());
            }
            index += 1;
        }
    }

    fn take_output(&mut self) -> Result<Option<DecodedFrame>, DecodeError> {
        // With a D3D manager the decoder allocates its (D3D11) samples.
        let provided = if self.provides_samples {
            None
        } else {
            // SAFETY: plain object creation.
            Some(unsafe {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(4 * 1024 * 1024)?)?;
                sample
            })
        };
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(provided),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0_u32;
        // SAFETY: one output buffer for stream 0; the MFT fills `pSample`
        // and may set `pEvents`; both are taken back below.
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };
        // SAFETY: each field is taken or dropped exactly once.
        let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
        unsafe { ManuallyDrop::drop(&mut output.pEvents) };
        match result {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                self.select_nv12_output()?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
        let decoded_at = qpc_now();
        let sample = sample
            .ok_or_else(|| DecodeError::Windows("ProcessOutput returned no sample".to_owned()))?;
        let size = self
            .size
            .ok_or_else(|| DecodeError::Windows("output without a picture size".to_owned()))?;
        // SAFETY: valid sample with one D3D11 buffer (DXVA output); the
        // texture and slice index stay valid while `sample` is held.
        let (texture, slice, sample_time) = unsafe {
            let buffer = sample.GetBufferByIndex(0)?;
            let dxgi: IMFDXGIBuffer = buffer.cast()?;
            let mut texture: Option<ID3D11Texture2D> = None;
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut texture as *mut _ as *mut _)?;
            let texture = texture.ok_or_else(|| {
                DecodeError::Unsupported("decoder output is not a D3D11 texture".to_owned())
            })?;
            (
                texture,
                dxgi.GetSubresourceIndex()?,
                sample.GetSampleTime()?,
            )
        };
        let identity = self.pending.take(sample_time).unwrap_or(PendingFrame {
            frame_id: u64::MAX,
            captured_at: QpcTimestamp::from_ticks(0),
            submitted_at: decoded_at,
        });
        Ok(Some(DecodedFrame {
            texture,
            slice,
            width: size.width,
            height: size.height,
            frame_id: identity.frame_id,
            captured_at: identity.captured_at,
            submitted_at: identity.submitted_at,
            decoded_at,
            _sample: sample,
        }))
    }
}

impl VideoDecoder for MediaFoundationDecoder {
    fn name(&self) -> &'static str {
        "media-foundation"
    }

    fn submit(
        &mut self,
        access_unit: &[u8],
        frame_id: u64,
        captured_at: QpcTimestamp,
    ) -> Result<DecodeSubmit, DecodeError> {
        if access_unit.is_empty() {
            return Err(DecodeError::InvalidInput("empty access unit".to_owned()));
        }
        let length = u32::try_from(access_unit.len())
            .map_err(|_| DecodeError::InvalidInput("access unit too large".to_owned()))?;
        let sample_time = self.next_sample * SAMPLE_STEP;
        // SAFETY: plain object creation; the buffer is locked only while the
        // access unit is copied into it (compressed data, not pixels).
        let sample = unsafe {
            let buffer = MFCreateMemoryBuffer(length)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(access_unit.as_ptr(), data, access_unit.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(length)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(sample_time)?;
            sample.SetSampleDuration(SAMPLE_STEP)?;
            sample
        };
        let submitted_at = qpc_now();
        // SAFETY: valid transform and sample.
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_NOTACCEPTING => return Ok(DecodeSubmit::Busy),
            Err(error) => return Err(error.into()),
        }
        self.pending.push(
            sample_time,
            PendingFrame {
                frame_id,
                captured_at,
                submitted_at,
            },
        );
        self.next_sample += 1;
        Ok(DecodeSubmit::Accepted)
    }

    fn poll(&mut self) -> Result<Option<DecodedFrame>, DecodeError> {
        // A stream change yields no frame; try once more for the picture.
        for _ in 0..2 {
            if let Some(frame) = self.take_output()? {
                return Ok(Some(frame));
            }
            if self.size.is_some() {
                break;
            }
        }
        Ok(None)
    }

    fn drain(&mut self) -> Result<Vec<DecodedFrame>, DecodeError> {
        // SAFETY: valid transform; asks it to output every pending picture.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }?;
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        let mut frames = Vec::new();
        while Instant::now() < deadline {
            match self.take_output() {
                Ok(Some(frame)) => frames.push(frame),
                Ok(None) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(frames)
    }
}

impl Drop for MediaFoundationDecoder {
    fn drop(&mut self) {
        // SAFETY: valid transform; ends streaming before release.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// The visible picture size: the minimum display aperture when the stream
/// crops (1080p is coded as 1088 rows), else the frame size.
fn picture_size(media_type: &IMFMediaType) -> Option<PictureSize> {
    let mut aperture = MFVideoArea::default();
    // SAFETY: valid media type; the blob is read into a struct of exactly
    // its size (`MFVideoArea`).
    let has_aperture = unsafe {
        media_type.GetBlob(
            &MF_MT_MINIMUM_DISPLAY_APERTURE,
            std::slice::from_raw_parts_mut(
                (&mut aperture as *mut MFVideoArea).cast::<u8>(),
                size_of::<MFVideoArea>(),
            ),
            None,
        )
    }
    .is_ok();
    if has_aperture && aperture.Area.cx > 0 && aperture.Area.cy > 0 {
        return Some(PictureSize {
            width: aperture.Area.cx as u32,
            height: aperture.Area.cy as u32,
        });
    }
    // SAFETY: valid media type; static key.
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }.ok()?;
    let (width, height) = ((packed >> 32) as u32, packed as u32);
    (width > 0 && height > 0).then_some(PictureSize { width, height })
}
