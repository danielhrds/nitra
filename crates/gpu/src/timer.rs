//! GPU execution time of a span of commands, measured with D3D11 timestamp
//! queries (not with the CPU time of the API calls, which only record work).
//!
//! Each measurement uses a disjoint query and two timestamps from a fixed
//! ring. Results are read later without stalling the CPU; when every slot is
//! still in flight, a measurement is skipped and counted, so the ring never
//! grows.

use std::collections::VecDeque;

use windows::Win32::Foundation::S_OK;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_QUERY_DATA_TIMESTAMP_DISJOINT, D3D11_QUERY_DESC,
    D3D11_QUERY_TIMESTAMP, D3D11_QUERY_TIMESTAMP_DISJOINT, ID3D11DeviceContext, ID3D11Query,
};
use windows::core::{Interface, Result};

use crate::device::D3d11Device;

struct Slot {
    disjoint: ID3D11Query,
    start: ID3D11Query,
    end: ID3D11Query,
}

/// Identifies a measurement between `begin` and `end`.
#[derive(Debug)]
#[must_use = "pass the token to GpuTimer::end"]
pub struct TimerToken(usize);

pub struct GpuTimer {
    slots: Vec<Slot>,
    free: Vec<usize>,
    in_flight: VecDeque<usize>,
    skipped: u64,
}

impl GpuTimer {
    /// Creates a timer with `capacity` measurements that can be in flight.
    pub fn new(device: &D3d11Device, capacity: usize) -> Result<Self> {
        let create = |kind| -> Result<ID3D11Query> {
            let desc = D3D11_QUERY_DESC {
                Query: kind,
                MiscFlags: 0,
            };
            let mut query = None;
            // SAFETY: valid device; descriptor and out-pointer reference live locals.
            unsafe { device.raw().CreateQuery(&desc, Some(&mut query)) }?;
            Ok(query.expect("CreateQuery succeeded"))
        };
        let slots = (0..capacity)
            .map(|_| {
                Ok(Slot {
                    disjoint: create(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
                    start: create(D3D11_QUERY_TIMESTAMP)?,
                    end: create(D3D11_QUERY_TIMESTAMP)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            free: (0..capacity).rev().collect(),
            slots,
            in_flight: VecDeque::with_capacity(capacity),
            skipped: 0,
        })
    }

    /// Starts a measurement; `None` (counted as skipped) if every slot is in flight.
    pub fn begin(&mut self, context: &ID3D11DeviceContext) -> Option<TimerToken> {
        let Some(index) = self.free.pop() else {
            self.skipped += 1;
            return None;
        };
        let slot = &self.slots[index];
        // SAFETY: queries belong to the context's device and are not in use.
        unsafe {
            context.Begin(&slot.disjoint);
            context.End(&slot.start);
        }
        Some(TimerToken(index))
    }

    /// Ends the measurement started with `token`.
    pub fn end(&mut self, context: &ID3D11DeviceContext, token: TimerToken) {
        let slot = &self.slots[token.0];
        // SAFETY: as in `begin`; the disjoint query was begun there.
        unsafe {
            context.End(&slot.end);
            context.End(&slot.disjoint);
        }
        self.in_flight.push_back(token.0);
    }

    /// Returns the GPU times (µs) of finished measurements, oldest first,
    /// without waiting. Measurements spanning a disjoint interval (e.g. a
    /// GPU clock change) are discarded.
    pub fn collect(&mut self, context: &ID3D11DeviceContext) -> Vec<f64> {
        let mut times = Vec::new();
        while let Some(&index) = self.in_flight.front() {
            match self.read(context, index) {
                Readiness::Pending => break,
                Readiness::Ready(time) => {
                    times.extend(time);
                    self.in_flight.pop_front();
                    self.free.push(index);
                }
            }
        }
        times
    }

    /// Waits for every measurement in flight (end of a run) and returns them.
    pub fn finish(&mut self, context: &ID3D11DeviceContext) -> Vec<f64> {
        // SAFETY: valid context; makes sure the queries reach the GPU.
        unsafe { context.Flush() };
        let mut times = Vec::new();
        while !self.in_flight.is_empty() {
            times.extend(self.collect(context));
            std::thread::yield_now();
        }
        times
    }

    /// Measurements skipped because every slot was in flight.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    fn read(&self, context: &ID3D11DeviceContext, index: usize) -> Readiness {
        let slot = &self.slots[index];
        let mut disjoint = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
        let (mut start, mut end) = (0_u64, 0_u64);
        // SAFETY: each output buffer is a live local of exactly the size the
        // query type writes.
        let ready = unsafe {
            get_data(context, &slot.disjoint, &mut disjoint)
                && get_data(context, &slot.start, &mut start)
                && get_data(context, &slot.end, &mut end)
        };
        if !ready {
            return Readiness::Pending;
        }
        if disjoint.Disjoint.as_bool() || disjoint.Frequency == 0 || end < start {
            return Readiness::Ready(None);
        }
        Readiness::Ready(Some(ticks_to_micros(end - start, disjoint.Frequency)))
    }
}

/// Non-blocking `GetData`: true when the query's data was written to `out`.
///
/// Called through the vtable because the generated wrapper maps both S_OK
/// (ready) and S_FALSE (not ready yet) to `Ok`, losing the distinction.
///
/// # Safety
/// `T` must be exactly the data type the query writes.
unsafe fn get_data<T>(context: &ID3D11DeviceContext, query: &ID3D11Query, out: &mut T) -> bool {
    // SAFETY: the vtable entry is ID3D11DeviceContext::GetData; both
    // interface pointers are valid, `out` is a live buffer of
    // `size_of::<T>()` bytes, and DONOTFLUSH keeps the call non-blocking.
    let hresult = unsafe {
        (Interface::vtable(context).GetData)(
            Interface::as_raw(context),
            Interface::as_raw(query),
            (out as *mut T).cast(),
            size_of::<T>() as u32,
            D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
        )
    };
    hresult == S_OK
}

enum Readiness {
    Pending,
    /// `None` when the measurement was invalid and discarded.
    Ready(Option<f64>),
}

fn ticks_to_micros(ticks: u64, frequency: u64) -> f64 {
    ticks as f64 * 1_000_000.0 / frequency as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DebugLayer, create_device, enumerate_adapters};

    #[test]
    fn gpu_ticks_convert_to_micros() {
        assert_eq!(ticks_to_micros(1_000, 1_000_000_000), 1.0);
        assert_eq!(ticks_to_micros(250, 100_000_000), 2.5);
    }

    #[test]
    #[ignore = "requires a GPU"]
    fn measures_without_growing() {
        let adapters = enumerate_adapters().unwrap();
        let adapter = adapters.iter().find(|a| !a.info.is_software).unwrap();
        let device = create_device(adapter, DebugLayer::IfAvailable).unwrap();
        let context = device.immediate_context().unwrap();
        let mut timer = GpuTimer::new(&device, 4).unwrap();

        let mut times = Vec::new();
        for _ in 0..100 {
            if let Some(token) = timer.begin(&context) {
                timer.end(&context, token);
            }
            times.extend(timer.collect(&context));
        }
        times.extend(timer.finish(&context));
        // Every begun measurement is either returned or discarded as disjoint.
        assert!(times.len() as u64 + timer.skipped() <= 100);
        assert!(!times.is_empty());
        assert!(
            times.iter().all(|time| (0.0..10_000.0).contains(time)),
            "{times:?}"
        );
        assert!(
            device
                .take_debug_messages()
                .iter()
                .all(|m| !m.severity.is_problem())
        );
    }
}
