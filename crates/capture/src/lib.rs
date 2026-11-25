//! Frame sources: WGC and DXGI Desktop Duplication capture, read-only
//! capture capability probes, a synthetic GPU frame generator and a test
//! window, and cameras. All capture backends share one handle type,
//! [`Capture`].

pub mod camera;
mod common;
pub mod dxgi;
pub mod probe;
pub mod select;
pub mod synthetic;
pub mod test_window;
pub mod wgc;

pub use common::{
    Capture, CaptureEnd, CaptureOptions, CaptureReport, CounterSnapshot, LatestSlot, RateLimiter,
    Taken,
};
