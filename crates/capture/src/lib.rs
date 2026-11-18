//! Frame sources: WGC and DXGI Desktop Duplication capture, read-only
//! capture capability probes, a synthetic GPU frame generator and a test
//! window. All capture backends share one handle type, [`Capture`].

mod common;
pub mod dxgi;
pub mod probe;
pub mod synthetic;
pub mod test_window;
pub mod wgc;

pub use common::{
    Capture, CaptureEnd, CaptureOptions, CaptureReport, CounterSnapshot, LatestSlot, RateLimiter,
    Taken,
};
