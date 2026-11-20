//! Audio for screen sharing: capture (WASAPI loopback of the system, of one
//! app, or a synthetic source), Opus encoding, and playback that follows
//! the sender's clock (drift correction). Audio never blocks video: every
//! queue here is bounded and drops instead of waiting.

pub mod capture;
pub mod drift;
pub mod frames;
pub mod opus;
pub mod player;
