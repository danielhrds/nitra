//! Audio for screen sharing: capture (WASAPI loopback of the system, of the
//! system without some apps, of one app, or a synthetic source), Opus
//! encoding, and playback that follows the sender's clock (drift
//! correction). Audio never blocks video: every queue here is bounded and
//! drops instead of waiting.

pub mod capture;
pub mod drift;
pub mod frames;
pub mod mixer;
pub mod opus;
pub mod player;
pub mod process;
mod sessions;
