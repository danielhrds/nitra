//! Turns a frame from any capture API into a delivered `GpuFrame`: rate
//! limit, copy into a pool texture (GPU to GPU), hand off, count, measure.

use gpu::{D3d11Device, FrameFormat, GpuFrame, TexturePool, create_texture_pool};
use telemetry::{DEFAULT_SAMPLE_CAPACITY, QpcDuration, QpcTimestamp, qpc_frequency, qpc_now};
use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, ID3D11DeviceContext, ID3D11Texture2D};

use super::{CaptureOptions, Counters, RateLimiter, ThreadContext};

/// Per-frame measurements, bounded by `DEFAULT_SAMPLE_CAPACITY`.
#[derive(Debug, Default)]
pub(crate) struct Samples {
    pub cost: Vec<QpcDuration>,
    pub frame_time_offset: Vec<i64>,
    pub intervals: Vec<QpcDuration>,
}

pub(crate) struct Delivery {
    context: ID3D11DeviceContext,
    textures: TexturePool,
    width: u32,
    height: u32,
    pool_size: usize,
    limiter: RateLimiter,
    next_frame_id: u64,
    last_delivery: Option<QpcTimestamp>,
    samples: Samples,
}

impl Delivery {
    pub fn new(
        device: &D3d11Device,
        options: &CaptureOptions,
        width: u32,
        height: u32,
    ) -> windows::core::Result<Self> {
        Ok(Self {
            context: device.immediate_context()?,
            textures: create_texture_pool(device, width.max(1), height.max(1), options.pool_size)?,
            width,
            height,
            pool_size: options.pool_size,
            limiter: RateLimiter::new(options.fps, qpc_frequency()),
            next_frame_id: 0,
            last_delivery: None,
            samples: Samples::default(),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Whether a frame arriving at `now` fits the target rate (counted if not).
    pub fn admit(&mut self, now: QpcTimestamp, counters: &Counters) -> bool {
        let admitted = self.limiter.admit(now);
        if !admitted {
            Counters::add(&counters.rate_dropped, 1);
        }
        admitted
    }

    /// Replaces the pool with textures of the new size. Leases of the old
    /// pool stay valid until their holders drop them.
    pub fn resize(
        &mut self,
        device: &D3d11Device,
        width: u32,
        height: u32,
        counters: &Counters,
    ) -> windows::core::Result<()> {
        self.textures = create_texture_pool(device, width.max(1), height.max(1), self.pool_size)?;
        self.width = width;
        self.height = height;
        Counters::add(&counters.resizes, 1);
        Ok(())
    }

    /// Copies the top-left `size()` region of `source` (BGRA8, same device)
    /// into a pool texture and hands it to the consumer slot. Returns whether
    /// the frame was delivered (false if every pool texture was in use).
    /// After releasing the API's frame, call [`Self::record_cost`].
    pub fn deliver(
        &mut self,
        source: &ID3D11Texture2D,
        captured_at: QpcTimestamp,
        context: &ThreadContext,
    ) -> bool {
        let counters = &context.counters;
        let Some(texture) = self.textures.try_acquire() else {
            Counters::add(&counters.pool_dropped, 1);
            return false;
        };
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: self.width,
            bottom: self.height,
            back: 1,
        };
        // SAFETY: both textures are BGRA8 on the same multithread-protected
        // device; the box lies in both (the pool matches `size()`, and the
        // caller checked the source is at least that big).
        unsafe {
            self.context
                .CopySubresourceRegion(&*texture, 0, 0, 0, 0, source, 0, Some(&region));
        }

        let frame = GpuFrame {
            texture,
            format: FrameFormat::Bgra8,
            width: self.width,
            height: self.height,
            frame_id: self.next_frame_id,
            captured_at,
        };
        self.next_frame_id += 1;
        if context.slot.put(frame) {
            Counters::add(&counters.replaced, 1);
        }
        Counters::add(&counters.delivered, 1);

        let handed_off = qpc_now();
        if self.samples.frame_time_offset.len() < DEFAULT_SAMPLE_CAPACITY {
            self.samples
                .frame_time_offset
                .push(handed_off.ticks() - captured_at.ticks());
            if let Some(previous) = self.last_delivery {
                self.samples
                    .intervals
                    .push(handed_off.duration_since(previous));
            }
        }
        self.last_delivery = Some(handed_off);
        true
    }

    /// Records the capture thread's time for a delivered frame, from
    /// `started` (when it began handling the frame) to now. Call it after
    /// the API's frame has been released, so every backend's cost includes
    /// acquire, copy, hand-off and release.
    pub fn record_cost(&mut self, started: QpcTimestamp) {
        if self.samples.cost.len() < DEFAULT_SAMPLE_CAPACITY {
            self.samples.cost.push(qpc_now().duration_since(started));
        }
    }

    pub fn into_samples(self) -> Samples {
        self.samples
    }
}
